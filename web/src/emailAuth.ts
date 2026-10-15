// Pure helpers for the "email + password" account UI (slice 3.8c): server
// error texts, client-side input checks and the resend timer. No DOM access.

/** Minimum wait between two `email_code_request`s for one address (server rule). */
export const RESEND_COOLDOWN_SECS = 60;

const EMAIL_ERROR_TEXTS: Record<string, string> = {
  "invalid email": "Неверный адрес почты",
  "weak password": "Пароль — от 8 до 128 символов",
  "wrong code": "Неверный или просроченный код",
  "wrong email or password": "Неверная почта или пароль",
  "email unavailable": "Не удалось отправить письмо — попробуйте позже",
  "too many attempts": "Слишком много попыток — подождите и попробуйте снова",
  "already registered": "У этого аккаунта уже есть почта",
};

/** Russian text for a server `error.message` during the email flow. */
export function emailErrorText(message: string): string {
  // Not importing app.ts (cycle): the generic fallback is duplicated here.
  return EMAIL_ERROR_TEXTS[message] ?? `Ошибка: ${message}`;
}

/** Error text for an obviously bad address, or `null` if it looks fine. */
export function validateEmail(email: string): string | null {
  const value = email.trim();
  const at = value.indexOf("@");
  const domain = at >= 0 ? value.slice(at + 1) : "";
  const ok =
    value.length > 0 &&
    at > 0 &&
    value.indexOf("@", at + 1) === -1 &&
    !/\s/.test(value) &&
    domain.includes(".") &&
    !domain.startsWith(".") &&
    !domain.endsWith(".");
  return ok ? null : emailErrorText("invalid email");
}

/** Error text for a password outside 8..=128 characters (code points). */
export function validatePassword(password: string): string | null {
  const len = Array.from(password).length;
  return len >= 8 && len <= 128 ? null : emailErrorText("weak password");
}

/** The code with whitespace removed (what gets sent to the server). */
export function normalizeCode(code: string): string {
  return code.replace(/\s+/g, "");
}

/** Error text unless the code is exactly six digits (spaces ignored). */
export function validateCode(code: string): string | null {
  return /^[0-9]{6}$/.test(normalizeCode(code)) ? null : emailErrorText("wrong code");
}

/** Whole seconds left until "send the code again" is allowed (0 = allowed
 * now), from the last send time; `null` last send means never sent. */
export function resendSecondsLeft(
  lastSentMs: number | null,
  nowMs: number,
  cooldownSecs: number = RESEND_COOLDOWN_SECS,
): number {
  if (lastSentMs === null) return 0;
  const left = Math.ceil((lastSentMs + cooldownSecs * 1000 - nowMs) / 1000);
  return Math.min(Math.max(left, 0), cooldownSecs);
}
