use rcdesk_server::admin::admin_emails_from_env;
use rcdesk_server::app::{app_with_admins, spawn_pin_rotation, Liveness};
use rcdesk_server::db::Db;
use rcdesk_server::ice::IceConfig;
use rcdesk_server::mail::mailer_from_env;
use rcdesk_server::maintenance::spawn_maintenance;
use rcdesk_server::passkeys;
use rcdesk_server::registry::Registry;

/// Что делать по аргументам командной строки.
#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    Run,
    Help,
    Version,
    Unknown(String),
}

/// Разбирает аргументы (без имени программы). Решает по первому аргументу;
/// остальные игнорируются.
fn parse_args(args: impl IntoIterator<Item = String>) -> CliAction {
    match args.into_iter().next() {
        None => CliAction::Run,
        Some(arg) => match arg.as_str() {
            "-h" | "--help" => CliAction::Help,
            "-V" | "--version" => CliAction::Version,
            _ => CliAction::Unknown(arg),
        },
    }
}

const HELP: &str = "\
rcdesk-server: сигнальный сервер rcdesk (удалённый рабочий стол через браузер).

Использование: rcdesk-server [-h | --help] [-V | --version]

Аргументов настройки нет: сервер настраивается только переменными окружения.

Переменные окружения:
  RCDESK_PORT              порт HTTP/WebSocket (по умолчанию 8080)
  RCDESK_DB_PATH           путь к файлу SQLite (по умолчанию rcdesk.db)
  RCDESK_ADMIN_EMAILS      почты администраторов через запятую
  RCDESK_STUN_URLS         STUN-адреса через запятую (по умолчанию stun:stun.l.google.com:19302)
  RCDESK_TURN_URLS         TURN-адреса через запятую; без них TURN отключён
  RCDESK_TURN_SECRET       общий секрет coturn (use-auth-secret) для временных учётных данных
  RCDESK_TURN_TTL_SECS     срок жизни учётных данных TURN, секунды (по умолчанию 7200)
  RCDESK_WEBAUTHN_ORIGIN   origin для passkeys (по умолчанию https://rcdesk.app)
  RCDESK_WEBAUTHN_RP_ID    RP ID для passkeys (по умолчанию rcdesk.app)
  RCDESK_MAIL              режим почты: smtp, log или пусто (отключена)
  RCDESK_MAIL_FROM         адрес отправителя при RCDESK_MAIL=smtp (по умолчанию rcdesk <noreply@rcdesk.app>)
  RCDESK_SMTP_HOST         SMTP-сервер (по умолчанию smtp.resend.com)
  RCDESK_SMTP_PORT         порт SMTP (по умолчанию 465)
  RCDESK_SMTP_USER         пользователь SMTP (по умолчанию resend)
  RCDESK_SMTP_PASSWORD     пароль SMTP; обязателен при RCDESK_MAIL=smtp
  RUST_LOG                 уровень логов tracing (по умолчанию info)
";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Аргументы разбираем до любых побочных эффектов (хук, логгер, БД).
    match parse_args(std::env::args().skip(1)) {
        CliAction::Run => {}
        CliAction::Help => {
            print!("{HELP}");
            return Ok(());
        }
        CliAction::Version => {
            println!("rcdesk-server {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        CliAction::Unknown(arg) => {
            eprintln!("rcdesk-server: неизвестный аргумент «{arg}»");
            eprintln!("см. rcdesk-server --help");
            std::process::exit(2);
        }
    }

    // S-12: паника в задаче tokio отравляет std-мьютексы реестра/БД
    // (`expect("... poisoned")` повсюду): `/healthz` отдаёт 503, но
    // `restart: unless-stopped` на unhealthy контейнер не реагирует. Поэтому
    // любая паника = abort, и Docker перезапускает процесс.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        prev_hook(info);
        std::process::abort();
    }));

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let port: u16 = std::env::var("RCDESK_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8080);

    let db_path = std::env::var("RCDESK_DB_PATH").unwrap_or_else(|_| "rcdesk.db".to_string());
    tracing::info!("opening database at {db_path}");
    let db = Db::open(std::path::Path::new(&db_path))?;

    // Долг D53: суточная уборка (см. `maintenance`).
    let registry = Registry::new();
    spawn_maintenance(db.clone(), registry.clone());

    spawn_pin_rotation(registry.clone(), Liveness::default().pin_ttl);

    let webauthn = passkeys::webauthn_from_env()?;

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!("rcdesk-server listening on {addr}");

    let shutdown_db = db.clone();
    let admin_emails = admin_emails_from_env();
    tracing::info!("admins configured: {}", admin_emails.len());
    let service = app_with_admins(
        registry,
        IceConfig::from_env(),
        db,
        webauthn,
        mailer_from_env()?,
        admin_emails,
    )
    .into_make_service_with_connect_info::<std::net::SocketAddr>();

    // Без with_graceful_shutdown: он ждал бы закрытия долгоживущих WebSocket.
    // Клиенты и хосты переподключаются сами, поэтому по сигналу просто выходим.
    tokio::select! {
        result = axum::serve(listener, service) => result?,
        () = shutdown_signal() => {
            tracing::info!("shutdown signal received");
            match shutdown_db.checkpoint() {
                Ok((0, _, _)) => {}
                Ok((busy, log, done)) => tracing::warn!(
                    "wal checkpoint incomplete: busy={busy} log={log} checkpointed={done}"
                ),
                Err(err) => tracing::warn!("wal checkpoint failed: {err:#}"),
            }
        }
    }

    Ok(())
}

/// Ждёт SIGTERM (docker stop, PID 1) или Ctrl+C; на не-unix — только Ctrl+C.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(err) => {
                tracing::warn!("failed to install SIGTERM handler: {err}");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> CliAction {
        parse_args(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn no_args_runs_server() {
        assert_eq!(parse(&[]), CliAction::Run);
    }

    #[test]
    fn help_flags() {
        assert_eq!(parse(&["--help"]), CliAction::Help);
        assert_eq!(parse(&["-h"]), CliAction::Help);
    }

    #[test]
    fn version_flags() {
        assert_eq!(parse(&["--version"]), CliAction::Version);
        assert_eq!(parse(&["-V"]), CliAction::Version);
    }

    #[test]
    fn unknown_arg() {
        assert_eq!(parse(&["--foo"]), CliAction::Unknown("--foo".into()));
    }

    #[test]
    fn first_arg_decides() {
        assert_eq!(parse(&["--help", "--foo"]), CliAction::Help);
        assert_eq!(
            parse(&["--foo", "--help"]),
            CliAction::Unknown("--foo".into())
        );
    }

    #[test]
    fn help_lists_every_variable() {
        for var in [
            "RCDESK_PORT",
            "RCDESK_DB_PATH",
            "RCDESK_ADMIN_EMAILS",
            "RCDESK_STUN_URLS",
            "RCDESK_TURN_URLS",
            "RCDESK_TURN_SECRET",
            "RCDESK_TURN_TTL_SECS",
            "RCDESK_WEBAUTHN_ORIGIN",
            "RCDESK_WEBAUTHN_RP_ID",
            "RCDESK_MAIL",
            "RCDESK_MAIL_FROM",
            "RCDESK_SMTP_HOST",
            "RCDESK_SMTP_PORT",
            "RCDESK_SMTP_USER",
            "RCDESK_SMTP_PASSWORD",
            "RUST_LOG",
        ] {
            assert!(HELP.contains(var), "в справке нет {var}");
        }
    }
}
