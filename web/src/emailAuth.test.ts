import { describe, expect, it } from "vitest";
import {
  FIX_ADDRESS_LABEL,
  canChangeEmail,
  emailChangeMessage,
  emailErrorText,
  normalizeCode,
  resendSecondsLeft,
  validateCode,
  validateCurrentPassword,
  validateEmail,
  validateNewEmail,
  validatePassword,
} from "./emailAuth";

describe("emailErrorText", () => {
  it("maps known server errors", () => {
    expect(emailErrorText("invalid email")).toBe("Неверный адрес почты");
    expect(emailErrorText("weak password")).toBe("Пароль — от 8 до 128 символов");
    expect(emailErrorText("wrong code")).toBe("Неверный или просроченный код");
    expect(emailErrorText("wrong email or password")).toBe("Неверная почта или пароль");
    expect(emailErrorText("email unavailable")).toBe(
      "Не удалось отправить письмо — попробуйте позже",
    );
    expect(emailErrorText("too many attempts")).toBe(
      "Слишком много попыток — подождите и попробуйте снова",
    );
    expect(emailErrorText("already registered")).toBe("У этого аккаунта уже есть почта");
  });
  it("maps the email change errors", () => {
    expect(emailErrorText("no account")).toBe("У аккаунта нет почты");
    expect(emailErrorText("same email")).toBe("Это текущая почта аккаунта");
    expect(emailErrorText("wrong password")).toBe("Неверный пароль");
    expect(emailErrorText("email taken")).toBe("Эта почта уже занята другим аккаунтом");
  });
  it("falls back to a generic text", () => {
    expect(emailErrorText("boom")).toBe("Ошибка: boom");
  });
});

describe("validateEmail", () => {
  it("accepts ordinary addresses (trimmed)", () => {
    expect(validateEmail("a@b.co")).toBeNull();
    expect(validateEmail("  user.name+x@mail.example.com ")).toBeNull();
  });
  it("rejects bad addresses", () => {
    for (const bad of ["", "   ", "abc", "a@b", "@b.co", "a@@b.co", "a b@c.de", "a@.co", "a@b."]) {
      expect(validateEmail(bad), bad).toBe("Неверный адрес почты");
    }
  });
});

describe("validatePassword", () => {
  it("accepts 8..128 code points", () => {
    expect(validatePassword("12345678")).toBeNull();
    expect(validatePassword("a".repeat(128))).toBeNull();
    expect(validatePassword("😀".repeat(8))).toBeNull();
  });
  it("rejects too short and too long", () => {
    expect(validatePassword("1234567")).toBe("Пароль — от 8 до 128 символов");
    expect(validatePassword("😀".repeat(7))).not.toBeNull();
    expect(validatePassword("a".repeat(129))).not.toBeNull();
  });
});

describe("code", () => {
  it("normalizes spaces", () => {
    expect(normalizeCode(" 123 456 ")).toBe("123456");
  });
  it("needs exactly six digits", () => {
    expect(validateCode("123456")).toBeNull();
    expect(validateCode("123 456")).toBeNull();
    expect(validateCode("12345")).not.toBeNull();
    expect(validateCode("1234567")).not.toBeNull();
    expect(validateCode("12345a")).not.toBeNull();
    expect(validateCode("")).not.toBeNull();
  });
});

describe("resendSecondsLeft", () => {
  it("is 0 when never sent", () => {
    expect(resendSecondsLeft(null, 1000)).toBe(0);
  });
  it("counts down and clamps", () => {
    expect(resendSecondsLeft(0, 0)).toBe(60);
    expect(resendSecondsLeft(0, 1)).toBe(60);
    expect(resendSecondsLeft(0, 30_000)).toBe(30);
    expect(resendSecondsLeft(0, 59_001)).toBe(1);
    expect(resendSecondsLeft(0, 60_000)).toBe(0);
    expect(resendSecondsLeft(0, 90_000)).toBe(0);
    expect(resendSecondsLeft(10_000, 0)).toBe(60);
  });
});

describe("email change helpers", () => {
  it("shows the change button only for an account with an email", () => {
    expect(canChangeEmail("a@b.co")).toBe(true);
    expect(canChangeEmail(null)).toBe(false);
    expect(canChangeEmail(undefined)).toBe(false);
  });

  it("rejects a bad new address and the current one (any case)", () => {
    expect(validateNewEmail("nope", "a@b.co")).toBe("Неверный адрес почты");
    expect(validateNewEmail(" A@B.co ", "a@b.co")).toBe("Это текущая почта аккаунта");
    expect(validateNewEmail("new@b.co", "a@b.co")).toBeNull();
  });

  it("requires a non-empty current password", () => {
    expect(validateCurrentPassword("")).toBe("Неверный пароль");
    expect(validateCurrentPassword("x")).toBeNull();
  });

  it("builds email_change with the code stripped of spaces", () => {
    expect(emailChangeMessage("new@b.co", "123 456", "pw")).toEqual({
      type: "email_change",
      email: "new@b.co",
      code: "123456",
      password: "pw",
    });
  });

  it("renames the wrong-address link", () => {
    expect(FIX_ADDRESS_LABEL).toBe("Исправить адрес");
  });
});
