//! Отправка писем с кодом подтверждения (слайс 3.8a). Настоящий SMTP --
//! следующий под-шаг (3.8b); пока есть заглушки: `DisabledMailer` (почта не
//! настроена) и `LogMailer` (код в лог, для локальной разработки).

use std::sync::Arc;

use futures_util::future::BoxFuture;

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

/// `RCDESK_MAIL=log` -- `LogMailer`, иначе `DisabledMailer`.
pub fn mailer_from_env() -> SharedMailer {
    match std::env::var("RCDESK_MAIL").as_deref() {
        Ok("log") => Arc::new(LogMailer),
        _ => Arc::new(DisabledMailer),
    }
}
