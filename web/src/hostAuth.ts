// Pure state machine behind `#auth-dialog` in `app.ts` (slice 3.2d): a host
// with an access password set answers `joined`/`peer_joined` with
// `auth_required` instead of `offer`, and the client must complete an
// OPAQUE login over the signaling relay (`pake_start`/`pake_response`/
// `pake_finish`, see `proto::signal::SignalMessage`) before it gets one. No
// DOM access here, same reasoning as `inputBlocked.ts`/`inputStatus.ts` --
// `app.ts` owns the DOM and the signaling wiring, this module owns the
// phases and their labels so they're testable without a browser or the real
// (slow, ~1s Argon2id) WASM module.

import * as opaque from "@serenity-kit/opaque";

/** The client half of one OPAQUE login attempt, narrowed from
 * `@serenity-kit/opaque`'s `client.startLogin`/`client.finishLogin` (see
 * that package's `index.d.ts`) to what `app.ts` needs. `state`/`request`/
 * `response`/`finalization` are opaque base64 (URL-safe, no padding)
 * strings -- `app.ts` forwards them as-is in `pake_start.payload` /
 * `pake_finish.payload` and reads `pake_response.payload` the same way, per
 * `SignalMessage`'s doc comments. `finishLogin` returns `null` for a wrong
 * password (the real implementation collapses both ways
 * `@serenity-kit/opaque` can report that -- an `undefined` result or a
 * thrown error, see `opaqueClient` below -- to this one case). */
export interface OpaqueClient {
  startLogin(password: string): Promise<{ state: string; request: string }>;
  finishLogin(
    state: string,
    response: string,
    password: string,
  ): Promise<{ finalization: string; sessionKey: string } | null>;
}

/** The real `OpaqueClient`, backed by `@serenity-kit/opaque` (the browser
 * side of the OPAQUE login `host/src/access.rs` runs on the other end --
 * see that module's doc comment for why both sides must agree on the exact
 * cipher suite and KSF parameters). `opaque.ready` resolves once the
 * package's inlined WASM has finished instantiating; awaited on every call
 * since nothing here tracks readiness across calls itself. */
export const opaqueClient: OpaqueClient = {
  async startLogin(password) {
    await opaque.ready;
    const { clientLoginState, startLoginRequest } = opaque.client.startLogin({ password });
    return { state: clientLoginState, request: startLoginRequest };
  },
  async finishLogin(state, response, password) {
    await opaque.ready;
    // `keyStretching: "memory-constrained"` must be passed explicitly to
    // match the host's `CustomKsf` (`host/src/access.rs`'s Argon2id m=2^16,
    // t=3, p=4) -- the package's own default differs and would silently
    // break interop. No `identifiers`: both sides agree on the fixed
    // `CREDENTIAL_ID` instead (see that constant's doc comment).
    try {
      const result = opaque.client.finishLogin({
        clientLoginState: state,
        loginResponse: response,
        password,
        keyStretching: "memory-constrained",
      });
      if (!result) return null;
      return { finalization: result.finishLoginRequest, sessionKey: result.sessionKey };
    } catch {
      // A `loginResponse` that doesn't check out against `state`/`password`
      // can throw instead of returning `undefined` (verified against the
      // real package in `hostAuth.opaque.test.ts`) -- both mean the same
      // thing to the caller: this login attempt failed.
      return null;
    }
  },
};

/** One state of the auth dialog. `prompt` is the resting state (password
 * field editable, submit enabled once it's non-empty) -- `message` is an
 * error to show above the field, or `null` right after `auth_required`/on a
 * fresh dialog. `starting`/`verifying`/`waiting-host` are the three steps of
 * one login attempt in flight (`startLogin` running, waiting for
 * `pake_response`+`finishLogin` running, waiting for the host's `offer`
 * after a successful `pake_finish`). `locked` is a host-imposed cooldown
 * (`auth_failed.retry_after_secs`) -- the field stays disabled until
 * `untilMs`. */
export type AuthPhase =
  | { kind: "prompt"; message: string | null }
  | { kind: "starting" }
  | { kind: "verifying" }
  | { kind: "waiting-host" }
  | { kind: "locked"; untilMs: number; message: string };

/** Turns an `auth_failed` message into the next `AuthPhase`. `retry_after_secs
 * === null` is a plain wrong-password-style rejection for *this* attempt --
 * the client may retry right away. `Some(n)` is the host's own lockout after
 * too many failures in a row: the field stays disabled until `nowMs +
 * n*1000` (see `lockedLabel`). */
export function authFailedPhase(retryAfterSecs: number | null, nowMs: number): AuthPhase {
  if (retryAfterSecs === null) {
    return { kind: "prompt", message: "Хост отверг попытку, попробуйте ещё раз" };
  }
  return {
    kind: "locked",
    untilMs: nowMs + retryAfterSecs * 1000,
    message: "Слишком много попыток",
  };
}

/** The countdown label for a `locked` phase, e.g. "Повторить через 12 с" --
 * rounded up so it never reads "через 0 с" while still locked, and never
 * goes below 1. */
export function lockedLabel(untilMs: number, nowMs: number): string {
  const remainingSecs = Math.max(1, Math.ceil((untilMs - nowMs) / 1000));
  return `Повторить через ${remainingSecs} с`;
}

/** The phase to show after `opaqueClient.finishLogin` returns `null` (a
 * wrong password caught locally, before any `pake_finish`/`auth_failed`
 * round trip). */
export function wrongPasswordPhase(): AuthPhase {
  return { kind: "prompt", message: "Неверный пароль" };
}

/** The status line text for `phase`. `nowMs` defaults to `Date.now()` for
 * callers that just want "the label right now" (`app.ts`'s per-phase-change
 * render); the per-second `locked` countdown timer passes it explicitly so
 * each tick's label reflects that tick's time exactly. */
export function phaseLabel(phase: AuthPhase, nowMs: number = Date.now()): string {
  switch (phase.kind) {
    case "starting":
      return "Подготовка…";
    case "verifying":
      return "Проверка пароля…";
    case "waiting-host":
      return "Ожидание хоста…";
    case "prompt":
      return phase.message ?? "";
    case "locked":
      return lockedLabel(phase.untilMs, nowMs);
  }
}

/** Whether the submit button/Enter-in-field should be active: only while
 * resting on `prompt` with something typed. */
export function canSubmit(phase: AuthPhase, password: string): boolean {
  return phase.kind === "prompt" && password.length > 0;
}
