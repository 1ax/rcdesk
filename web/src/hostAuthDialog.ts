/**
 * The host-password dialog (`#auth-dialog`): the OPAQUE login that runs
 * between `auth_required` and a successful `pake_finish`, plus the session
 * key it derives. A feature factory split out of `app.ts`'s `mount()`: it
 * finds its own DOM in `root`, keeps its state in the closure and talks to
 * the rest of `mount()` only through `HostAuthDialogDeps` and the returned
 * `HostAuthDialog`.
 */
import type { SignalMessage } from "./generated/SignalMessage";
import type { SignalingClient } from "./signaling";
import { trapFocus } from "./focusTrap";
import {
  authFailedPhase,
  canSubmit,
  opaqueClient,
  phaseLabel,
  wrongPasswordPhase,
} from "./hostAuth";
import type { AuthPhase } from "./hostAuth";

type Msg<T extends SignalMessage["type"]> = Extract<SignalMessage, { type: T }>;

export interface HostAuthDialogDeps {
  /** The signaling connection (`pake_start`/`pake_finish`/`bye` go out through it). */
  signaling: SignalingClient;
  /** The `mount()` root; the dialog looks its own elements up here. */
  root: Element;
  /** The id of the current session (`mount()`'s `sessionId`), `null` outside one. */
  getSessionId: () => string | null;
  /** Ends the session locally (`mount()`'s `teardown`); used by cancel. */
  teardown: (reason: string) => void;
  /** Slice E2: remembers that this host requires a password (`saveAuthRequired` for the current device). */
  rememberAuthRequired: () => void;
}

export interface HostAuthDialog {
  /** `joined`: saves the host's name shown in the dialog. */
  setHostName: (name: string) => void;
  /** `auth_required` for the current session: opens the dialog at the password prompt. */
  onAuthRequired: (msg: Msg<"auth_required">) => void;
  /** `pake_response`: finishes the login and sends `pake_finish`. */
  onPakeResponse: (msg: Msg<"pake_response">) => void;
  /** `auth_failed`: shows the wrong-password/locked state. */
  onAuthFailed: (msg: Msg<"auth_failed">) => void;
  /** The OPAQUE session key of a completed login, `null` without one (DTLS binding). */
  sessionKey: () => string | null;
  /** `offer` arrived: closes the dialog if it is still up. */
  closeOnOffer: () => void;
  /** The session's resources are being dropped: clears all auth state and hides the dialog. */
  resetSession: () => void;
}

export function createHostAuthDialog(deps: HostAuthDialogDeps): HostAuthDialog {
  const { signaling, root, getSessionId, teardown, rememberAuthRequired } =
    deps;
  // Slice 3.2d: the OPAQUE-login dialog, shown between `auth_required` and
  // either a successful `pake_finish` (the host then sends `offer`, see that
  // handler below) or a cancel/teardown.
  const authDialog = root.querySelector<HTMLDivElement>("#auth-dialog")!;
  const authHostNameEl =
    root.querySelector<HTMLParagraphElement>("#auth-host-name")!;
  const authPasswordInput =
    root.querySelector<HTMLInputElement>("#auth-password")!;
  const authStatusEl =
    root.querySelector<HTMLParagraphElement>("#auth-status")!;
  const authSubmitBtn = root.querySelector<HTMLButtonElement>("#auth-submit")!;
  const authCancelBtn = root.querySelector<HTMLButtonElement>("#auth-cancel")!;

  // Slice 3.2d: `null` while `#auth-dialog` is hidden (no OPAQUE login in
  // flight or pending); set by `auth_required` and cleared once `offer`
  // arrives, the login is cancelled, or the session ends. Drives
  // `renderAuthDialog`.
  let authPhase: AuthPhase | null = null;
  // The host name from the current session's `joined` (slice 3.2d) --
  // shown in the auth dialog so the owner knows which device is asking.
  let authHostName: string | null = null;
  // The password typed for the *current* login attempt, held only between
  // `startLogin` (which needs it again for `finishLogin`) and that
  // `finishLogin` call -- never kept around longer than one attempt needs.
  let authPassword: string | null = null;
  // `opaqueClient.startLogin`'s `state`, needed by the matching
  // `finishLogin` once `pake_response` arrives; `null` outside that window.
  let authState: string | null = null;
  // The session key `finishLogin` derived on a successful login -- not used
  // by this slice (3.2e wires it into the data channel), only stored and
  // reset alongside the rest of the auth state.
  let authSessionKey: string | null = null;
  // Ticks once a second while `authPhase.kind === "locked"`, re-rendering
  // the countdown and flipping back to `prompt` once it expires.
  let authLockTimer: ReturnType<typeof setInterval> | undefined;

  /** Stops the per-second `locked`-countdown tick (`authLockTimer`), if
   * running -- called whenever `authPhase` moves away from `locked`, and by
   * every full auth-state reset. */
  function stopAuthLockTimer(): void {
    if (authLockTimer !== undefined) {
      clearInterval(authLockTimer);
      authLockTimer = undefined;
    }
  }

  /** Clears the password/`clientLoginState` pair of one OPAQUE login attempt
   * (slice 3.2d) and stops the `locked` countdown timer -- called after
   * every attempt settles, one way or another (a wrong password caught
   * locally, an `auth_failed` from the host, a successful `pake_finish`, or
   * the whole session ending). Leaves `authHostName`/`authPhase`/the
   * dialog's visibility to the caller -- those live for the whole dialog
   * (or session), not just one attempt. */
  function resetAuthAttempt(): void {
    stopAuthLockTimer();
    authPassword = null;
    authState = null;
  }

  /** Renders `#auth-dialog` from `authPhase`: hidden when `null`, otherwise
   * the status line (`phaseLabel`), the field's `disabled` state (editable
   * only at rest, on `prompt`), and the submit button's `disabled` state
   * (`canSubmit`). Called on every `authPhase` change and on the password
   * field's own `input` event, since `canSubmit` also depends on what's
   * typed. */
  function renderAuthDialog(): void {
    authDialog.hidden = authPhase === null;
    if (authPhase === null) return;
    authHostNameEl.textContent = authHostName ?? "";
    authStatusEl.textContent = phaseLabel(authPhase);
    const editable = authPhase.kind === "prompt";
    authPasswordInput.disabled = !editable;
    authSubmitBtn.disabled = !canSubmit(authPhase, authPasswordInput.value);
  }

  /** Starts one OPAQUE login attempt (slice 3.2d): `opaqueClient.startLogin`
   * (the ~1s Argon2id "memory-constrained" KSF, see `hostAuth.ts`), then
   * `pake_start` over signaling. Only runs from `authPhase.kind ===
   * "prompt"` with a submittable password (`canSubmit`) -- the click/Enter
   * handlers below already check that, this is the defensive inner check.
   * `requestSessionId` guards the awaited `startLogin` against a session
   * that ended (cancel, `bye`, lost connection) while it was running -- its
   * result would otherwise resurrect auth state for a dialog that's already
   * gone. */
  async function submitAuthLogin(): Promise<void> {
    const sessionId = getSessionId();
    if (authPhase === null || authPhase.kind !== "prompt" || sessionId === null)
      return;
    const password = authPasswordInput.value;
    if (!canSubmit(authPhase, password)) return;
    const requestSessionId = sessionId;
    authPhase = { kind: "starting" };
    renderAuthDialog();
    let started: { state: string; request: string };
    try {
      started = await opaqueClient.startLogin(password);
    } catch (err) {
      console.error("failed to start OPAQUE login", err);
      if (getSessionId() !== requestSessionId) return;
      authPhase = {
        kind: "prompt",
        message: "Не удалось загрузить модуль входа",
      };
      renderAuthDialog();
      return;
    }
    if (getSessionId() !== requestSessionId) return;
    authState = started.state;
    authPassword = password;
    signaling.send({
      type: "pake_start",
      session_id: requestSessionId,
      payload: started.request,
    });
    authPhase = { kind: "verifying" };
    renderAuthDialog();
  }

  /** Cancels the auth dialog (slice 3.2d): same path as `#disconnect-btn`
   * (`bye` if a session is open, then `teardown`) so the device list
   * refreshes the same way. Used by `#auth-cancel` and Esc in the password
   * field. */
  function cancelAuthDialog(): void {
    const sessionId = getSessionId();
    if (sessionId) {
      signaling.send({ type: "bye", session_id: sessionId });
    }
    teardown("Подключение отменено");
  }

  // Slice 3.2d: submitting the auth dialog, by button or by Enter in the
  // password field -- both just start `submitAuthLogin`, which re-checks
  // `canSubmit` itself (the button is only enabled when it's true, but the
  // field's `keydown` isn't gated the same way).
  authSubmitBtn.addEventListener("click", () => {
    void submitAuthLogin();
  });
  authPasswordInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      void submitAuthLogin();
    } else if (event.key === "Escape") {
      event.preventDefault();
      cancelAuthDialog();
    }
  });
  // `canSubmit` depends on what's typed, not just `authPhase` -- re-render
  // on every keystroke so the submit button enables/disables live.
  authPasswordInput.addEventListener("input", () => {
    renderAuthDialog();
  });
  authCancelBtn.addEventListener("click", () => {
    cancelAuthDialog();
  });

  // UI-4: keep Tab inside the password dialog while it is open.
  authDialog.addEventListener("keydown", (event) => {
    if (authDialog.hidden) return;
    trapFocus(authDialog, event);
  });

  return {
    setHostName: (name) => {
      authHostName = name;
    },
    onAuthRequired: (msg) => {
      if (msg.session_id !== getSessionId()) return;
      // Slice E2: remember that this host requires a password, so a later
      // `offer` without a completed login (a MITM swallowing `auth_required`)
      // is refused.
      rememberAuthRequired();
      resetAuthAttempt();
      authSessionKey = null;
      authPhase = { kind: "prompt", message: null };
      authPasswordInput.value = "";
      renderAuthDialog();
      authPasswordInput.focus();
    },
    onPakeResponse: (msg) => {
      if (msg.session_id !== getSessionId()) return;
      if (authState === null || authPassword === null) return;
      const state = authState;
      const password = authPassword;
      const requestSessionId = msg.session_id;
      void (async () => {
        let result: Awaited<ReturnType<typeof opaqueClient.finishLogin>>;
        try {
          result = await opaqueClient.finishLogin(state, msg.payload, password);
        } catch (err) {
          // Internal failure (e.g. the WASM module failed to load) -- not a
          // wrong password.
          console.error("OPAQUE finishLogin failed", err);
          if (getSessionId() !== requestSessionId) return;
          resetAuthAttempt();
          authPhase = {
            kind: "prompt",
            message: "Ошибка проверки пароля — попробуйте ещё раз",
          };
          authPasswordInput.value = "";
          renderAuthDialog();
          authPasswordInput.focus();
          return;
        }
        if (getSessionId() !== requestSessionId) return;
        if (result === null) {
          resetAuthAttempt();
          authPhase = wrongPasswordPhase();
          authPasswordInput.value = "";
          renderAuthDialog();
          authPasswordInput.focus();
          return;
        }
        // Slice 3.2e: kept until the session ends (see
        // `stopSessionResources`) and read below, in `signaling.on("offer")`/
        // `session.acceptOffer(...).then(...)`, to bind the offer/answer to
        // this session key.
        authSessionKey = result.sessionKey;
        resetAuthAttempt();
        signaling.send({
          type: "pake_finish",
          session_id: requestSessionId,
          payload: result.finalization,
        });
        authPhase = { kind: "waiting-host" };
        renderAuthDialog();
      })();
    },
    onAuthFailed: (msg) => {
      if (msg.session_id !== getSessionId()) return;
      resetAuthAttempt();
      authSessionKey = null;
      authPhase = authFailedPhase(msg.retry_after_secs, Date.now());
      authPasswordInput.value = "";
      renderAuthDialog();
      if (authPhase.kind === "locked") {
        authLockTimer = setInterval(() => {
          if (authPhase === null || authPhase.kind !== "locked") {
            stopAuthLockTimer();
            return;
          }
          if (Date.now() >= authPhase.untilMs) {
            stopAuthLockTimer();
            authPhase = { kind: "prompt", message: null };
          }
          renderAuthDialog();
        }, 1000);
      } else {
        authPasswordInput.focus();
      }
    },
    sessionKey: () => authSessionKey,
    closeOnOffer: () => {
      // Slice 3.2d: a `joined`/`auth_required` host only sends `offer` once
      // the OPAQUE login has succeeded (`pake_finish` accepted) -- close the
      // dialog if it's still up rather than leaving it stuck on
      // "Ожидание хоста…" behind the now-connecting session.
      if (authPhase !== null) {
        resetAuthAttempt();
        authPhase = null;
        renderAuthDialog();
      }
    },
    resetSession: () => {
      resetAuthAttempt();
      authHostName = null;
      authSessionKey = null;
      authPhase = null;
      renderAuthDialog();
    },
  };
}
