//! Отправка писем с кодом подтверждения (слайсы 3.8a, 3.8b): `SmtpMailer`
//! (прод), `DisabledMailer` (почта не настроена) и `LogMailer` (код в лог, для
//! локальной разработки).

use std::sync::Arc;

use anyhow::Context;
use futures_util::future::BoxFuture;
use lettre::message::{Mailbox, Message, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

pub trait Mailer: Send + Sync {
    /// Отправляет письмо с кодом подтверждения на адрес `to`.
    fn send_code(&self, to: &str, code: &str) -> BoxFuture<'static, anyhow::Result<()>>;
}

pub type SharedMailer = Arc<dyn Mailer>;

/// Почта не настроена: любая отправка -- ошибка.
pub struct DisabledMailer;

impl Mailer for DisabledMailer {
    fn send_code(&self, _to: &str, _code: &str) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async { Err(anyhow::anyhow!("mail is not configured")) })
    }
}

/// Пишет адрес и код в лог (`warn`, чтобы был виден при дефолтном фильтре).
/// Только для локальной разработки.
pub struct LogMailer;

impl Mailer for LogMailer {
    fn send_code(&self, to: &str, code: &str) -> BoxFuture<'static, anyhow::Result<()>> {
        tracing::warn!(%to, %code, "RCDESK_MAIL=log: email code (not sent)");
        Box::pin(async { Ok(()) })
    }
}

const DEFAULT_SMTP_HOST: &str = "smtp.resend.com";
const DEFAULT_SMTP_PORT: u16 = 465;
const DEFAULT_SMTP_USER: &str = "resend";
const DEFAULT_MAIL_FROM: &str = "rcdesk <noreply@rcdesk.app>";

fn code_body(code: &str) -> String {
    format!(
        "Ваш код для rcdesk: {code}\n\nКод действует 10 минут. Если вы не запрашивали код, просто проигнорируйте это письмо.\n"
    )
}

/// Собирает письмо с кодом. Отдельно от отправки, чтобы проверять без сети.
pub fn build_code_message(from: &Mailbox, to: &str, code: &str) -> anyhow::Result<Message> {
    let to: Mailbox = to.parse().context("invalid recipient address")?;
    Message::builder()
        .from(from.clone())
        .to(to)
        .subject(format!("Код rcdesk: {code}"))
        .singlepart(SinglePart::plain(code_body(code)))
        .context("failed to build email")
}

/// Отправка через SMTP (implicit TLS, rustls + webpki-roots).
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl SmtpMailer {
    pub fn new(
        host: &str,
        port: u16,
        user: String,
        password: String,
        from: Mailbox,
    ) -> anyhow::Result<Self> {
        let transport = AsyncSmtpTransport::<Tokio1Executor>::relay(host)
            .with_context(|| format!("invalid SMTP host {host:?}"))?
            .port(port)
            .credentials(Credentials::new(user, password))
            .build();
        Ok(Self { transport, from })
    }
}

impl Mailer for SmtpMailer {
    fn send_code(&self, to: &str, code: &str) -> BoxFuture<'static, anyhow::Result<()>> {
        let message = build_code_message(&self.from, to, code);
        let transport = self.transport.clone();
        Box::pin(async move {
            let message = message?;
            transport.send(message).await.context("SMTP send failed")?;
            Ok(())
        })
    }
}

/// `RCDESK_MAIL`: `smtp` -- `SmtpMailer`, `log` -- `LogMailer`, пусто --
/// `DisabledMailer`. Неверная настройка -- ошибка (сервер не стартует).
pub fn mailer_from_env() -> anyhow::Result<SharedMailer> {
    mailer_from_vars(|k| std::env::var(k).ok())
}

/// Разбор настроек почты от поставщика переменных (для тестов без env).
pub fn mailer_from_vars(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<SharedMailer> {
    let nonempty = |k: &str| get(k).filter(|v| !v.is_empty());
    let mode = nonempty("RCDESK_MAIL").unwrap_or_default();
    match mode.as_str() {
        "" => {
            tracing::info!("mail mode: disabled");
            Ok(Arc::new(DisabledMailer))
        }
        "log" => {
            tracing::info!("mail mode: log");
            Ok(Arc::new(LogMailer))
        }
        "smtp" => {
            let host = nonempty("RCDESK_SMTP_HOST").unwrap_or_else(|| DEFAULT_SMTP_HOST.into());
            let port = match nonempty("RCDESK_SMTP_PORT") {
                Some(p) => p
                    .parse::<u16>()
                    .with_context(|| format!("invalid RCDESK_SMTP_PORT {p:?}"))?,
                None => DEFAULT_SMTP_PORT,
            };
            let user = nonempty("RCDESK_SMTP_USER").unwrap_or_else(|| DEFAULT_SMTP_USER.into());
            let password = nonempty("RCDESK_SMTP_PASSWORD")
                .context("RCDESK_SMTP_PASSWORD is required when RCDESK_MAIL=smtp")?;
            let from_raw = nonempty("RCDESK_MAIL_FROM").unwrap_or_else(|| DEFAULT_MAIL_FROM.into());
            let from: Mailbox = from_raw
                .parse()
                .with_context(|| format!("invalid RCDESK_MAIL_FROM {from_raw:?}"))?;
            let mailer = SmtpMailer::new(&host, port, user, password, from)?;
            tracing::info!(%host, port, "mail mode: smtp");
            Ok(Arc::new(mailer))
        }
        other => {
            anyhow::bail!("unknown RCDESK_MAIL value {other:?} (expected smtp, log or empty)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lettre::message::header::Subject;
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn err_text(r: anyhow::Result<SharedMailer>) -> String {
        match r {
            Ok(_) => panic!("expected error"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn empty_and_log_modes_ok() {
        assert!(mailer_from_vars(vars(&[])).is_ok());
        assert!(mailer_from_vars(vars(&[("RCDESK_MAIL", "log")])).is_ok());
    }

    #[tokio::test]
    async fn smtp_minimal_ok() {
        let r = mailer_from_vars(vars(&[
            ("RCDESK_MAIL", "smtp"),
            ("RCDESK_SMTP_PASSWORD", "re_secret"),
        ]));
        assert!(r.is_ok());
    }

    #[test]
    fn smtp_requires_password() {
        let e = err_text(mailer_from_vars(vars(&[("RCDESK_MAIL", "smtp")])));
        assert!(e.contains("RCDESK_SMTP_PASSWORD"), "{e}");
    }

    #[test]
    fn smtp_rejects_bad_from() {
        let e = err_text(mailer_from_vars(vars(&[
            ("RCDESK_MAIL", "smtp"),
            ("RCDESK_SMTP_PASSWORD", "re_secret"),
            ("RCDESK_MAIL_FROM", "not an address"),
        ])));
        assert!(e.contains("RCDESK_MAIL_FROM"), "{e}");
        assert!(!e.contains("re_secret"));
    }

    #[test]
    fn smtp_rejects_bad_port() {
        for bad in ["abc", "70000", "-1"] {
            let e = err_text(mailer_from_vars(vars(&[
                ("RCDESK_MAIL", "smtp"),
                ("RCDESK_SMTP_PASSWORD", "re_secret"),
                ("RCDESK_SMTP_PORT", bad),
            ])));
            assert!(e.contains("RCDESK_SMTP_PORT"), "{e}");
        }
    }

    #[test]
    fn unknown_mode_is_error() {
        let e = err_text(mailer_from_vars(vars(&[("RCDESK_MAIL", "smtps")])));
        assert!(e.contains("RCDESK_MAIL"), "{e}");
    }

    #[test]
    fn message_has_subject_from_to_and_plain_utf8() {
        let from: Mailbox = DEFAULT_MAIL_FROM.parse().unwrap();
        let msg = build_code_message(&from, "user@example.com", "123456").unwrap();
        let subject = msg.headers().get::<Subject>().unwrap();
        assert_eq!(subject.as_ref(), "Код rcdesk: 123456");
        let env = msg.envelope();
        assert_eq!(env.from().unwrap().to_string(), "noreply@rcdesk.app");
        assert_eq!(env.to().len(), 1);
        assert_eq!(env.to()[0].to_string(), "user@example.com");
        let raw = String::from_utf8_lossy(&msg.formatted()).to_lowercase();
        assert!(
            raw.contains("content-type: text/plain; charset=utf-8"),
            "{raw}"
        );
        assert!(code_body("123456").contains("Ваш код для rcdesk: 123456"));
        assert!(code_body("123456").contains("Код действует 10 минут."));
    }

    #[test]
    fn message_rejects_bad_recipient() {
        let from: Mailbox = DEFAULT_MAIL_FROM.parse().unwrap();
        assert!(build_code_message(&from, "nope", "123456").is_err());
    }
}
