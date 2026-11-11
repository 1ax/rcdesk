/**
 * The owner's account: the first-screen account row, the cabinet (passkeys,
 * browsers, sign-out), the email + password card and the passkey flows. A
 * feature factory split out of `app.ts`'s `mount()`: it finds its own DOM in
 * `root`, keeps its state in the closure and talks to the rest of `mount()`
 * only through `AccountPanelDeps` and the returned `AccountPanel`.
 */
import type { SignalMessage } from "./generated/SignalMessage";
import type { BrowserEntry } from "./generated/BrowserEntry";
import type { PasskeyEntry } from "./generated/PasskeyEntry";
import type { SignalingClient } from "./signaling";
import type { DevicesPanel } from "./devicesPanel";
import { lanHubUi } from "./lanHub";
import {
  accountRowMode,
  assertionToServer,
  browserRowText,
  creationOptionsFromServer,
  isCancelError,
  type PasskeyFlow,
  passkeyFailText,
  passkeyRowText,
  registrationToServer,
  requestOptionsFromServer,
} from "./passkey";
import {
  canChangeEmail,
  emailChangeMessage,
  emailCodeConfirmMessage,
  emailErrorTextFor,
  resendSecondsLeft,
  validateCode,
  validateCurrentPassword,
  validateEmail,
  validateNewEmail,
  validatePassword,
} from "./emailAuth";
import { clearOwnerToken } from "./myDevices";

type Msg<T extends SignalMessage["type"]> = Extract<SignalMessage, { type: T }>;

export interface AccountPanelDeps {
  /** The signaling connection (account/passkey/email messages go out through it). */
  signaling: SignalingClient;
  /** The `mount()` root; the panel looks its own elements up here. */
  root: Element;
  /** True once the signaling socket closed (every account affordance is disabled). */
  isConnectionLost: () => boolean;
  /** The owner's `Storage` (module-level in `app.ts`), `null` when unavailable. */
  ownerStorage: () => Storage | null;
  /** The device list: `resetOwner` empties and redraws it. */
  devicesPanel: DevicesPanel;
}

export interface AccountPanel {
  /** `authenticated.lan_hub`: stores the flag and hides the account UI of a LAN hub. */
  setLanHub: (lanHub: boolean) => void;
  /** Rest of the `authenticated` handler after the LAN-hub flag: finishes a passkey login,
   * requests passkeys/account (and browsers while the cabinet is open). */
  onAuthenticated: () => void;
  /** The signaling socket closed: drops the pending auth request and redraws the card. */
  onConnectionClosed: () => void;
  /** Shows the first screen again (PIN screen): closes the cabinet. */
  closeAccount: () => void;
  /** Back to the first screen as a fresh anonymous owner (`signed_out`, revoked token). */
  resetOwner: () => void;
  /** Server `error` while an email request or passkey flow is in flight: shows it; true when consumed. */
  onError: (message: string) => boolean;
  onEmailCodeSent: () => void;
  onAccount: (msg: Msg<"account">) => void;
  onPasskeyRegisterOptions: (msg: Msg<"passkey_register_options">) => void;
  onPasskeyLoginOptions: (msg: Msg<"passkey_login_options">) => void;
  onPasskeys: (msg: Msg<"passkeys">) => void;
  onBrowsers: (msg: Msg<"browsers">) => void;
}

export function createAccountPanel(deps: AccountPanelDeps): AccountPanel {
  const { signaling, root, ownerStorage, devicesPanel } = deps;
  const mainCard = root.querySelector<HTMLDivElement>("#main-card")!;
  const accountEl = root.querySelector<HTMLDivElement>("#account")!;
  const accountGuestEl = root.querySelector<HTMLDivElement>("#account-guest")!;
  const accountOpenBtn = root.querySelector<HTMLButtonElement>("#account-open-btn")!;
  const accountStatusEl = root.querySelector<HTMLParagraphElement>("#account-status")!;
  const passkeyLoginBtn = root.querySelector<HTMLButtonElement>("#passkey-login-btn")!;
  const loginOpenBtn = root.querySelector<HTMLButtonElement>("#login-open-btn")!;
  const signupOpenBtn = root.querySelector<HTMLButtonElement>("#signup-open-btn")!;
  const emailBlock = root.querySelector<HTMLDivElement>("#email-block")!;
  const emailCurrentEl = root.querySelector<HTMLParagraphElement>("#email-current")!;
  const emailActionBtn = root.querySelector<HTMLButtonElement>("#email-action-btn")!;
  const emailChangeBtn = root.querySelector<HTMLButtonElement>("#email-change-btn")!;
  const passkeyBlock = root.querySelector<HTMLDivElement>("#passkey-block")!;
  const authCard = root.querySelector<HTMLDivElement>("#auth-card")!;
  const eaTitle = root.querySelector<HTMLHeadingElement>("#ea-title")!;
  const eaLoginForm = root.querySelector<HTMLFormElement>("#ea-login-form")!;
  const eaLoginEmail = root.querySelector<HTMLInputElement>("#ea-login-email")!;
  const eaLoginPassword = root.querySelector<HTMLInputElement>("#ea-login-password")!;
  const eaLoginSubmit = root.querySelector<HTMLButtonElement>("#ea-login-submit")!;
  const eaForgotBtn = root.querySelector<HTMLButtonElement>("#ea-forgot-btn")!;
  const eaSignupBtn = root.querySelector<HTMLButtonElement>("#ea-signup-btn")!;
  const eaPasskeyLoginBtn = root.querySelector<HTMLButtonElement>("#ea-passkey-login-btn")!;
  const eaRequestForm = root.querySelector<HTMLFormElement>("#ea-request-form")!;
  const eaRequestEmail = root.querySelector<HTMLInputElement>("#ea-request-email")!;
  const eaRequestSubmit = root.querySelector<HTMLButtonElement>("#ea-request-submit")!;
  const eaToLoginBtn = root.querySelector<HTMLButtonElement>("#ea-to-login-btn")!;
  const eaPasskeySaveBtn = root.querySelector<HTMLButtonElement>("#ea-passkey-save-btn")!;
  const eaConfirmForm = root.querySelector<HTMLFormElement>("#ea-confirm-form")!;
  const eaSentText = root.querySelector<HTMLParagraphElement>("#ea-sent-text")!;
  const eaCode = root.querySelector<HTMLInputElement>("#ea-code")!;
  const eaNewPassword = root.querySelector<HTMLInputElement>("#ea-new-password")!;
  const eaConfirmSubmit = root.querySelector<HTMLButtonElement>("#ea-confirm-submit")!;
  const eaResendBtn = root.querySelector<HTMLButtonElement>("#ea-resend-btn")!;
  const eaChangeEmailBtn = root.querySelector<HTMLButtonElement>("#ea-change-email-btn")!;
  const eaStatus = root.querySelector<HTMLParagraphElement>("#ea-status")!;
  const eaBackBtn = root.querySelector<HTMLButtonElement>("#ea-back-btn")!;
  const accountCard = root.querySelector<HTMLDivElement>("#account-card")!;
  const passkeyListEl = root.querySelector<HTMLUListElement>("#passkey-list")!;
  const passkeyAddBtn = root.querySelector<HTMLButtonElement>("#passkey-add-btn")!;
  const browserListEl = root.querySelector<HTMLUListElement>("#browser-list")!;
  const signOutBtn = root.querySelector<HTMLButtonElement>("#sign-out-btn")!;
  const signOutHintEl = root.querySelector<HTMLParagraphElement>("#sign-out-hint")!;
  const accountCardStatusEl = root.querySelector<HTMLParagraphElement>("#account-card-status")!;
  const accountBackBtn = root.querySelector<HTMLButtonElement>("#account-back-btn")!;
  const adminLink = root.querySelector<HTMLAnchorElement>("#admin-link")!;

  // Slice 3.7c: the owner's passkeys (`null` until `passkeys` answers the
  // `list_passkeys` sent after every `authenticated`) and browsers (cabinet).
  let passkeys: PasskeyEntry[] | null = null;
  let browsers: BrowserEntry[] = [];
  // The passkey flow in progress (between the button press and the final
  // `passkeys`/`authenticated`/error), or `null`; buttons are disabled while set.
  let passkeyFlow: PasskeyFlow | null = null;
  // Cabinet two-click confirmation key (`passkey:<id>`, `browser:<id>`,
  // `sign-out`), reset by the document click listener like `confirmingForgetId`.
  let confirmingAccountKey: string | null = null;
  // Slice 3.8c: the owner's account email (`undefined` until `account`
  // answers the `get_account` sent after every `authenticated`; `null` = none).
  let accountEmail: string | null | undefined = undefined;
  // D47: the server is a LAN hub (`authenticated.lan_hub`) -- no accounts,
  // passkeys, mail or agent download; their UI is hidden (`applyLanHub`).
  let lanHub = false;
  // The email auth card: "login" form or the "code" flow (registration /
  // password reset / password change, told apart by `authLabel`; "change" is
  // the email change of a signed-in account), its step,
  // the address the code went to, the request in flight (buttons disabled
  // while set; server errors go to the card while set) and the resend timer.
  let authMode: "login" | "code" | "change" = "login";
  let authLabel = "Вход";
  let authStep: 1 | 2 = 1;
  let authEmail = "";
  let authPending: "request" | "confirm" | "login" | "change" | null = null;
  let authLastSentMs: number | null = null;
  let authTimer: ReturnType<typeof setInterval> | undefined;
  const passkeySupported =
    typeof window !== "undefined" &&
    typeof window.PublicKeyCredential !== "undefined" &&
    typeof navigator !== "undefined" &&
    !!navigator.credentials;

  // ---- Slice 3.7c: account row, cabinet, passkey flows ----

  function setAccountMessage(text: string): void {
    accountStatusEl.textContent = text;
    accountCardStatusEl.textContent = text;
    eaStatus.textContent = text;
  }

  /** D47: in LAN-hub mode forces the account/passkey/email/download UI
   * hidden (`hidden` attribute, like the rest of the client). Idempotent;
   * a no-op for a regular server, so their own rendering stays as is. */
  function applyLanHub(): void {
    const ui = lanHubUi(lanHub);
    if (!ui.showAccountUi) {
      accountEl.hidden = true;
      passkeyLoginBtn.hidden = true;
      emailBlock.hidden = true;
      passkeyBlock.hidden = true;
      authCard.hidden = true;
    }
    if (!ui.showDownload) {
      const downloadEl = root!.querySelector<HTMLElement>("#download");
      if (downloadEl) downloadEl.hidden = true;
    }
  }

  /** Shows the first-screen account row for the current `passkeys`. */
  function renderAccountRow(): void {
    const mode = accountRowMode(passkeySupported, passkeys, accountEmail);
    accountEl.hidden = mode === "hidden";
    accountGuestEl.hidden = mode !== "guest";
    accountOpenBtn.hidden = mode !== "account";
    passkeyLoginBtn.hidden = !passkeySupported;
    passkeyBlock.hidden = !passkeySupported;
    applyLanHub();
    const busy = passkeyFlow !== null || deps.isConnectionLost();
    passkeyLoginBtn.disabled = busy;
    passkeyAddBtn.disabled = busy;
    loginOpenBtn.disabled = deps.isConnectionLost();
    signupOpenBtn.disabled = deps.isConnectionLost();
    emailActionBtn.disabled = deps.isConnectionLost() || authPending !== null;
    emailChangeBtn.disabled = deps.isConnectionLost() || authPending !== null;
    renderAuth();
  }

  // ---- Slice 3.8c: email + password card ----

  function stopAuthTimer(): void {
    if (authTimer !== undefined) clearInterval(authTimer);
    authTimer = undefined;
  }

  /** Shows the auth card for the current state (mode, step, pending request). */
  function renderAuth(): void {
    const busy = authPending !== null || deps.isConnectionLost();
    const registering = authMode === "code" && authLabel === "Регистрация";
    eaTitle.textContent = authLabel;
    eaLoginForm.hidden = authMode !== "login";
    eaRequestForm.hidden = !(authMode !== "login" && authStep === 1);
    eaConfirmForm.hidden = !(authMode !== "login" && authStep === 2);
    // Email change: the second field is the CURRENT password, not a new one.
    const changing = authMode === "change";
    eaRequestEmail.placeholder = changing ? "Новая почта" : "Почта";
    eaNewPassword.placeholder = changing ? "Текущий пароль" : "Новый пароль (8–128 символов)";
    eaNewPassword.autocomplete = changing ? "current-password" : "new-password";
    eaLoginSubmit.disabled = busy;
    eaRequestSubmit.disabled = busy;
    eaConfirmSubmit.disabled = busy;
    eaForgotBtn.disabled = busy;
    eaSignupBtn.disabled = busy;
    eaToLoginBtn.disabled = busy;
    eaChangeEmailBtn.disabled = busy;
    eaPasskeyLoginBtn.hidden = !passkeySupported;
    eaPasskeyLoginBtn.disabled = busy || passkeyFlow !== null;
    eaPasskeySaveBtn.hidden = !passkeySupported || !registering;
    eaPasskeySaveBtn.disabled = busy || passkeyFlow !== null;
    eaToLoginBtn.hidden = !registering;
    eaSentText.textContent = `Код отправлен на ${authEmail}`;
    const left = resendSecondsLeft(authLastSentMs, Date.now());
    eaResendBtn.textContent =
      left > 0 ? `Отправить код ещё раз (${left} с)` : "Отправить код ещё раз";
    eaResendBtn.disabled = busy || left > 0;
    if (left === 0) stopAuthTimer();
  }

  function openAuth(mode: "login" | "code" | "change", label: string, email: string): void {
    stopAuthTimer();
    authMode = mode;
    authLabel = label;
    authStep = 1;
    authEmail = email;
    authPending = null;
    authLastSentMs = null;
    eaLoginEmail.value = email;
    eaRequestEmail.value = email;
    eaLoginPassword.value = "";
    eaCode.value = "";
    eaNewPassword.value = "";
    confirmingAccountKey = null;
    setAccountMessage("");
    mainCard.hidden = true;
    accountCard.hidden = true;
    authCard.hidden = false;
    renderAuth();
  }

  /** Back from the auth card to the cabinet (email change started there). */
  function showCabinet(): void {
    mainCard.hidden = true;
    accountCard.hidden = false;
  }

  function closeAuth(): void {
    stopAuthTimer();
    authPending = null;
    authLastSentMs = null;
    eaLoginPassword.value = "";
    eaCode.value = "";
    eaNewPassword.value = "";
    authCard.hidden = true;
    mainCard.hidden = !accountCard.hidden;
  }

  /** Sends `email_code_request` for `email` (first send or resend). */
  function requestEmailCode(email: string): void {
    authEmail = email;
    authPending = "request";
    setAccountMessage("");
    renderAuth();
    signaling.send({ type: "email_code_request", email });
  }

  /** A server error while an auth request was in flight. */
  function failAuth(message: string): void {
    authPending = null;
    setAccountMessage(emailErrorTextFor(message, authLabel));
    renderAuth();
  }

  eaLoginForm.addEventListener("submit", (event) => {
    event.preventDefault();
    if (authPending !== null || deps.isConnectionLost()) return;
    const email = eaLoginEmail.value.trim();
    const invalid = validateEmail(email) ?? validatePassword(eaLoginPassword.value);
    if (invalid !== null) {
      setAccountMessage(invalid);
      return;
    }
    authEmail = email;
    authPending = "login";
    setAccountMessage("");
    renderAuth();
    signaling.send({ type: "password_login", email, password: eaLoginPassword.value });
  });
  eaRequestForm.addEventListener("submit", (event) => {
    event.preventDefault();
    if (authPending !== null || deps.isConnectionLost()) return;
    const invalid =
      authMode === "change"
        ? validateNewEmail(eaRequestEmail.value, accountEmail)
        : validateEmail(eaRequestEmail.value);
    if (invalid !== null) {
      setAccountMessage(invalid);
      return;
    }
    requestEmailCode(eaRequestEmail.value.trim());
  });
  eaConfirmForm.addEventListener("submit", (event) => {
    event.preventDefault();
    if (authPending !== null || deps.isConnectionLost()) return;
    const changing = authMode === "change";
    const invalid =
      validateCode(eaCode.value) ??
      (changing
        ? validateCurrentPassword(eaNewPassword.value)
        : validatePassword(eaNewPassword.value));
    if (invalid !== null) {
      setAccountMessage(invalid);
      return;
    }
    authPending = changing ? "change" : "confirm";
    setAccountMessage("");
    renderAuth();
    if (changing) {
      signaling.send(emailChangeMessage(authEmail, eaCode.value, eaNewPassword.value));
      return;
    }
    signaling.send(
      emailCodeConfirmMessage(authLabel, authEmail, eaCode.value, eaNewPassword.value),
    );
  });
  eaForgotBtn.addEventListener("click", () =>
    openAuth("code", "Сброс пароля", eaLoginEmail.value.trim()),
  );
  eaSignupBtn.addEventListener("click", () =>
    openAuth("code", "Регистрация", eaLoginEmail.value.trim()),
  );
  eaToLoginBtn.addEventListener("click", () => openAuth("login", "Вход", eaRequestEmail.value.trim()));
  eaResendBtn.addEventListener("click", () => {
    if (authPending === null && resendSecondsLeft(authLastSentMs, Date.now()) === 0) {
      requestEmailCode(authEmail);
    }
  });
  eaChangeEmailBtn.addEventListener("click", () => {
    stopAuthTimer();
    authStep = 1;
    authLastSentMs = null;
    eaRequestEmail.value = authEmail;
    setAccountMessage("");
    renderAuth();
  });
  eaBackBtn.addEventListener("click", () => {
    const toCabinet = authMode === "change";
    closeAuth();
    if (toCabinet) showCabinet();
    setAccountMessage("");
  });
  loginOpenBtn.addEventListener("click", () => openAuth("login", "Вход", ""));
  signupOpenBtn.addEventListener("click", () => openAuth("code", "Регистрация", ""));
  eaPasskeyLoginBtn.addEventListener("click", () => startPasskeyFlow("login"));
  eaPasskeySaveBtn.addEventListener("click", () => startPasskeyFlow("register"));
  emailActionBtn.addEventListener("click", () => {
    if (typeof accountEmail === "string") openAuth("code", "Смена пароля", accountEmail);
    else openAuth("code", "Регистрация", "");
  });
  emailChangeBtn.addEventListener("click", () => {
    if (canChangeEmail(accountEmail)) openAuth("change", "Смена почты", "");
  });

  function onEmailCodeSent(): void {
    if (authPending !== "request") return;
    authPending = null;
    authStep = 2;
    authLastSentMs = Date.now();
    stopAuthTimer();
    authTimer = setInterval(renderAuth, 1000);
    setAccountMessage("");
    renderAuth();
    eaCode.focus();
  }

  function onAccount(msg: Msg<"account">): void {
    accountEmail = msg.email;
    adminLink.hidden = !msg.is_admin;
    if (authPending === "change" && msg.email !== null) {
      // Email changed: back to the cabinet with a note.
      closeAuth();
      showCabinet();
      setAccountMessage("Почта изменена");
      renderAccount();
      return;
    }
    const finishing = authPending === "confirm" || authPending === "login";
    if (finishing && msg.email !== null) {
      // Registration / reset / login done: back to the first screen.
      closeAuth();
      closeAccount();
      setAccountMessage(`Вы вошли как ${msg.email}`);
    }
    renderAccount();
  }

  function accountRowEl(
    title: string,
    status: string,
    button: HTMLButtonElement | null,
  ): HTMLLIElement {
    const li = document.createElement("li");
    li.className = "device-item";
    const info = document.createElement("div");
    info.className = "device-info";
    const nameEl = document.createElement("span");
    nameEl.className = "device-name";
    nameEl.textContent = title;
    const statusEl = document.createElement("span");
    statusEl.className = "device-status";
    statusEl.textContent = status;
    info.appendChild(nameEl);
    info.appendChild(statusEl);
    li.appendChild(info);
    if (button) {
      const actions = document.createElement("div");
      actions.className = "device-actions";
      actions.appendChild(button);
      li.appendChild(actions);
    }
    return li;
  }

  /** A "Удалить"-style button that needs a second click ("Точно?"). */
  function confirmButton(
    key: string,
    label: string,
    onConfirm: () => void,
  ): HTMLButtonElement {
    const btn = document.createElement("button");
    btn.className = "btn-secondary account-danger";
    btn.dataset.confirmKey = key;
    btn.textContent = confirmingAccountKey === key ? "Точно?" : label;
    btn.addEventListener("click", () => {
      if (confirmingAccountKey === key) {
        confirmingAccountKey = null;
        onConfirm();
      } else {
        confirmingAccountKey = key;
      }
      renderAccount();
    });
    return btn;
  }

  /** Rebuilds the cabinet lists and the sign-out button. */
  function renderAccount(): void {
    const nowSecs = Math.floor(Date.now() / 1000);
    passkeyListEl.innerHTML = "";
    for (const entry of passkeys ?? []) {
      const text = passkeyRowText(entry, nowSecs);
      passkeyListEl.appendChild(
        accountRowEl(
          text.title,
          text.status,
          confirmButton(`passkey:${entry.id}`, "Удалить", () =>
            signaling.send({ type: "delete_passkey", id: entry.id }),
          ),
        ),
      );
    }
    browserListEl.innerHTML = "";
    for (const entry of browsers) {
      const text = browserRowText(entry, nowSecs);
      browserListEl.appendChild(
        accountRowEl(
          text.title,
          text.status,
          entry.current
            ? null
            : confirmButton(`browser:${entry.id}`, "Отозвать", () =>
                signaling.send({ type: "revoke_browser", id: entry.id }),
              ),
        ),
      );
    }
    emailBlock.hidden = accountEmail === undefined;
    emailCurrentEl.textContent = typeof accountEmail === "string" ? accountEmail : "";
    emailCurrentEl.hidden = typeof accountEmail !== "string";
    emailActionBtn.textContent =
      typeof accountEmail === "string" ? "Сменить пароль" : "Добавить почту и пароль";
    emailChangeBtn.hidden = !canChangeEmail(accountEmail);
    const needConfirm = (passkeys ?? []).length === 0 && typeof accountEmail !== "string";
    const confirmingSignOut = confirmingAccountKey === "sign-out";
    signOutBtn.dataset.confirmKey = "sign-out";
    signOutBtn.textContent = needConfirm && confirmingSignOut ? "Точно выйти?" : "Выйти";
    signOutHintEl.textContent =
      needConfirm && confirmingSignOut ? "Список компьютеров этого браузера пропадёт" : "";
    renderAccountRow();
  }

  function openAccount(): void {
    confirmingAccountKey = null;
    setAccountMessage("");
    mainCard.hidden = true;
    accountCard.hidden = false;
    renderAccount();
    signaling.send({ type: "list_passkeys" });
    signaling.send({ type: "get_account" });
    signaling.send({ type: "list_browsers" });
  }

  function closeAccount(): void {
    confirmingAccountKey = null;
    accountCard.hidden = true;
    mainCard.hidden = false;
  }

  /** Back to the first screen as a fresh anonymous owner: used after
   * `signed_out` and when this browser's token was revoked elsewhere. */
  function resetOwner(): void {
    const storage = ownerStorage();
    if (storage) clearOwnerToken(storage);
    passkeyFlow = null;
    passkeys = null;
    accountEmail = undefined;
    adminLink.hidden = true;
    browsers = [];
    devicesPanel.setDevices([]);
    closeAuth();
    closeAccount();
    setAccountMessage("");
    devicesPanel.render();
    renderAccountRow();
    signaling.send({ type: "client_auth", token: null });
  }

  function startPasskeyFlow(flow: PasskeyFlow): void {
    if (passkeyFlow !== null || !passkeySupported) return;
    passkeyFlow = flow;
    setAccountMessage("");
    renderAccount();
    signaling.send({ type: flow === "login" ? "passkey_login_start" : "passkey_register_start" });
  }

  /** A flow step failed: silent for a dismissed system dialog, otherwise a
   * "Не удалось …" next to the buttons. */
  function failPasskeyFlow(err: unknown): void {
    const flow = passkeyFlow;
    passkeyFlow = null;
    if (flow !== null && !isCancelError(err)) {
      console.warn("passkey flow failed", err);
      setAccountMessage(passkeyFailText(flow));
    }
    renderAccount();
  }

  passkeyLoginBtn.addEventListener("click", () => startPasskeyFlow("login"));
  passkeyAddBtn.addEventListener("click", () => startPasskeyFlow("register"));
  accountOpenBtn.addEventListener("click", openAccount);
  accountBackBtn.addEventListener("click", closeAccount);
  signOutBtn.addEventListener("click", () => {
    const needConfirm = (passkeys ?? []).length === 0 && typeof accountEmail !== "string";
    if (needConfirm && confirmingAccountKey !== "sign-out") {
      confirmingAccountKey = "sign-out";
      renderAccount();
      return;
    }
    confirmingAccountKey = null;
    signaling.send({ type: "sign_out" });
  });

  // Resets a pending cabinet "Точно?" when something else is clicked.
  document.addEventListener("click", (event) => {
    if (confirmingAccountKey === null) return;
    const target = event.target as Element | null;
    if (target?.closest(`[data-confirm-key="${CSS.escape(confirmingAccountKey)}"]`)) return;
    confirmingAccountKey = null;
    renderAccount();
  });

  function onPasskeyRegisterOptions(msg: Msg<"passkey_register_options">): void {
    if (passkeyFlow !== "register") return;
    void (async () => {
      try {
        const publicKey = creationOptionsFromServer(msg.options);
        const cred = await navigator.credentials.create({ publicKey });
        if (!cred) throw new Error("no credential");
        signaling.send({
          type: "passkey_register_finish",
          credential: registrationToServer(cred as unknown as Parameters<typeof registrationToServer>[0]),
        });
      } catch (err) {
        failPasskeyFlow(err);
      }
    })();
  }

  function onPasskeyLoginOptions(msg: Msg<"passkey_login_options">): void {
    if (passkeyFlow !== "login") return;
    void (async () => {
      try {
        const publicKey = requestOptionsFromServer(msg.options);
        const cred = await navigator.credentials.get({ publicKey });
        if (!cred) throw new Error("no credential");
        signaling.send({
          type: "passkey_login_finish",
          credential: assertionToServer(cred as unknown as Parameters<typeof assertionToServer>[0]),
        });
      } catch (err) {
        failPasskeyFlow(err);
      }
    })();
  }

  function onPasskeys(msg: Msg<"passkeys">): void {
    passkeys = msg.passkeys;
    if (passkeyFlow === "register") {
      passkeyFlow = null;
      // Saved through the registration card: nothing more to do there.
      if (!authCard.hidden) closeAuth();
    }
    renderAccount();
  }

  function onBrowsers(msg: Msg<"browsers">): void {
    browsers = msg.browsers;
    renderAccount();
  }

  return {
    setLanHub(value) {
      lanHub = value;
      applyLanHub();
    },
    onAuthenticated() {
      // Slice 3.7c: a passkey login finished (or an ordinary (re)auth): learn
      // whether this owner has passkeys, which drives the account row.
      if (passkeyFlow === "login") {
        passkeyFlow = null;
        if (!authCard.hidden) closeAuth();
      }
      if (lanHubUi(lanHub).requestAccountData) {
        signaling.send({ type: "list_passkeys" });
        signaling.send({ type: "get_account" });
      }
      if (!accountCard.hidden) signaling.send({ type: "list_browsers" });
    },
    onConnectionClosed() {
      authPending = null;
      renderAuth();
    },
    closeAccount,
    resetOwner,
    onError(message) {
      // Slice 3.8c: an email-flow request failed -- shown in the auth card.
      if (authPending !== null) {
        failAuth(message);
        return true;
      }
      // A passkey flow step failed on the server (`passkey failed` or other).
      if (passkeyFlow !== null) {
        failPasskeyFlow(new Error(message));
        return true;
      }
      return false;
    },
    onEmailCodeSent,
    onAccount,
    onPasskeyRegisterOptions,
    onPasskeyLoginOptions,
    onPasskeys,
    onBrowsers,
  };
}
