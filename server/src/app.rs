use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;

use proto::signal::SignalMessage;

use crate::accounts::EmailCodes;
use crate::db::Db;
use crate::ice::IceConfig;
use crate::ip_limit::{GlobalLimiter, IpLimiter};
use crate::mail::SharedMailer;
use crate::passkeys::SharedWebauthn;
use crate::registry::Registry;
use crate::ws::ws_handler;

/// Wrong-PIN guesses allowed in a burst per IP (slice 3.6a).
pub const PIN_BURST: u32 = 10;
pub const PIN_REFILL: Duration = Duration::from_secs(60);
/// Global wrong-PIN bucket (D2, audit S-H4): 300 attempts in a burst, then one
/// per 2 s, about 43k per day per server against 10^6 codes. Bound devices
/// connect via `ConnectDevice` without a PIN, so draining the bucket does not
/// affect them.
pub const PIN_GLOBAL_BURST: u32 = 300;
pub const PIN_GLOBAL_REFILL: Duration = Duration::from_secs(2);
/// New anonymous owners an IP may create in a burst (B2, audit S-H2).
pub const NEW_OWNER_BURST: u32 = 20;
pub const NEW_OWNER_REFILL: Duration = Duration::from_secs(60);
/// New devices an IP may register in a burst (B2, audit S-H2).
pub const NEW_DEVICE_BURST: u32 = 10;
pub const NEW_DEVICE_REFILL: Duration = Duration::from_secs(600);
/// Кодов подтверждения почты, которые IP может запросить в burst (слайс 3.8a).
pub const EMAIL_CODE_BURST: u32 = 5;
pub const EMAIL_CODE_REFILL: Duration = Duration::from_secs(600);
/// Неверных паролей/кодов почты, которые IP может потратить в burst
/// (слайс 3.8a).
pub const LOGIN_BURST: u32 = 10;
pub const LOGIN_REFILL: Duration = Duration::from_secs(60);

/// Серверный keepalive (B3): Ping раз в `ping_interval`, закрытие соединения,
/// если `read_timeout` не пришло ни одного кадра (текст, Ping, Pong — любой).
/// Хост сам пингует каждые 20 с, браузер отвечает Pong автоматически, так что
/// живое соединение всегда шлёт кадры чаще 90 с.
#[derive(Debug, Clone, Copy)]
pub struct Liveness {
    pub ping_interval: Duration,
    pub read_timeout: Duration,
    /// TTL PIN (D44): ротация только для хостов с `pin_rotation`.
    pub pin_ttl: Duration,
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(30),
            read_timeout: Duration::from_secs(90),
            pin_ttl: Duration::from_secs(600),
        }
    }
}

/// Shared state handed to every `/ws` connection: the in-memory signaling
/// registry, the ICE server configuration used to fill `Registered`'s and
/// `Joined`'s `ice_servers` field, and the persistent device/owner store.
#[derive(Clone)]
pub struct AppState {
    pub registry: Registry,
    pub ice: IceConfig,
    pub db: Db,
    /// Per-IP limit on wrong-PIN guesses (slice 3.6a).
    pub pin_limiter: IpLimiter,
    /// Server-wide limit on wrong-PIN guesses (D1).
    pub pin_global_limiter: GlobalLimiter,
    /// Per-IP limit on creating new owners (B2).
    pub owner_limiter: IpLimiter,
    /// Per-IP limit on registering new devices (B2).
    pub device_limiter: IpLimiter,
    /// WebAuthn relying party for passkeys (slice 3.7b).
    pub webauthn: SharedWebauthn,
    /// Server keepalive parameters (B3).
    pub liveness: Liveness,
    /// Отправка писем с кодом подтверждения (слайс 3.8a).
    pub mailer: SharedMailer,
    /// Выданные коды подтверждения почты, в памяти.
    pub email_codes: EmailCodes,
    /// Per-IP limit on issued email codes; spent by each issued code.
    pub email_code_limiter: IpLimiter,
    /// Per-IP limit on wrong passwords / email codes; spent by failures.
    pub login_limiter: IpLimiter,
    /// Режим «LAN-хаб» (D47): аккаунтов, passkey и почты нет; клиент узнаёт
    /// об этом из `Authenticated.lan_hub` и прячет их UI.
    pub lan_hub: bool,
    /// Почты администраторов (A3): trim + lowercase, как в `accounts`. В
    /// LAN-хабе всегда пусто.
    pub admin_emails: Arc<Vec<String>>,
}

pub fn app(
    registry: Registry,
    ice: IceConfig,
    db: Db,
    webauthn: SharedWebauthn,
    mailer: SharedMailer,
) -> Router {
    app_with_liveness(registry, ice, db, webauthn, mailer, Liveness::default())
}

/// Как [`app`], но с администраторами (A3): `admin_emails` -- почты уже в
/// нижнем регистре (см. `admin::parse_admin_emails`).
pub fn app_with_admins(
    registry: Registry,
    ice: IceConfig,
    db: Db,
    webauthn: SharedWebauthn,
    mailer: SharedMailer,
    admin_emails: Vec<String>,
) -> Router {
    build_app(
        registry,
        ice,
        db,
        webauthn,
        mailer,
        Liveness::default(),
        GlobalLimiter::new(PIN_GLOBAL_BURST, PIN_GLOBAL_REFILL),
        false,
        admin_emails,
    )
}

pub fn app_with_liveness(
    registry: Registry,
    ice: IceConfig,
    db: Db,
    webauthn: SharedWebauthn,
    mailer: SharedMailer,
    liveness: Liveness,
) -> Router {
    app_with_limits(
        registry,
        ice,
        db,
        webauthn,
        mailer,
        liveness,
        GlobalLimiter::new(PIN_GLOBAL_BURST, PIN_GLOBAL_REFILL),
    )
}

/// Полная конфигурация (тесты): лимиты, liveness и администраторы.
#[allow(clippy::too_many_arguments)]
pub fn app_full(
    registry: Registry,
    ice: IceConfig,
    db: Db,
    webauthn: SharedWebauthn,
    mailer: SharedMailer,
    liveness: Liveness,
    pin_global: GlobalLimiter,
    admin_emails: Vec<String>,
) -> Router {
    build_app(
        registry,
        ice,
        db,
        webauthn,
        mailer,
        liveness,
        pin_global,
        false,
        admin_emails,
    )
}

pub fn app_with_limits(
    registry: Registry,
    ice: IceConfig,
    db: Db,
    webauthn: SharedWebauthn,
    mailer: SharedMailer,
    liveness: Liveness,
    pin_global: GlobalLimiter,
) -> Router {
    build_app(
        registry,
        ice,
        db,
        webauthn,
        mailer,
        liveness,
        pin_global,
        false,
        Vec::new(),
    )
}

/// Как [`app`] (лимиты и liveness по умолчанию), но сервер объявляет клиентам
/// режим «LAN-хаб» (D47): `Authenticated.lan_hub == true`.
pub fn lan_hub_app(
    registry: Registry,
    ice: IceConfig,
    db: Db,
    webauthn: SharedWebauthn,
    mailer: SharedMailer,
) -> Router {
    build_app(
        registry,
        ice,
        db,
        webauthn,
        mailer,
        Liveness::default(),
        GlobalLimiter::new(PIN_GLOBAL_BURST, PIN_GLOBAL_REFILL),
        true,
        Vec::new(),
    )
}

#[allow(clippy::too_many_arguments)]
fn build_app(
    registry: Registry,
    ice: IceConfig,
    db: Db,
    webauthn: SharedWebauthn,
    mailer: SharedMailer,
    liveness: Liveness,
    pin_global: GlobalLimiter,
    lan_hub: bool,
    admin_emails: Vec<String>,
) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_handler))
        .with_state(AppState {
            registry,
            ice,
            db,
            pin_limiter: IpLimiter::new(PIN_BURST, PIN_REFILL),
            pin_global_limiter: pin_global,
            owner_limiter: IpLimiter::new(NEW_OWNER_BURST, NEW_OWNER_REFILL),
            device_limiter: IpLimiter::new(NEW_DEVICE_BURST, NEW_DEVICE_REFILL),
            webauthn,
            liveness,
            mailer,
            email_codes: EmailCodes::new(),
            email_code_limiter: IpLimiter::new(EMAIL_CODE_BURST, EMAIL_CODE_REFILL),
            login_limiter: IpLimiter::new(LOGIN_BURST, LOGIN_REFILL),
            lan_hub,
            admin_emails: Arc::new(admin_emails),
        })
}

/// Фоновая ротация PIN по TTL (D44): на каждом тике выдаёт новый PIN хостам с
/// `pin_rotation`, чей PIN просрочен, и шлёт им `PinChanged`.
pub fn spawn_pin_rotation(registry: Registry, pin_ttl: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let period = (pin_ttl / 10).max(Duration::from_millis(100));
        let mut interval = tokio::time::interval(period);
        loop {
            interval.tick().await;
            let rotated = registry.rotate_expired_pins(std::time::Instant::now(), pin_ttl);
            if !rotated.is_empty() {
                tracing::info!("rotated {} expired pins", rotated.len());
            }
            for (tx, pin) in rotated {
                let _ = tx.send(SignalMessage::PinChanged { pin });
            }
        }
    })
}

/// Liveness plus a check that the DB and the registry mutexes are usable.
/// The body of the success response must stay exactly `ok` (deploy checks it).
async fn healthz(State(state): State<AppState>) -> (StatusCode, &'static str) {
    if let Err(err) = state.db.ping() {
        tracing::error!(?err, "healthz: db check failed");
        return (StatusCode::SERVICE_UNAVAILABLE, "db error");
    }
    if !state.registry.is_healthy() {
        tracing::error!("healthz: registry mutex poisoned");
        return (StatusCode::SERVICE_UNAVAILABLE, "registry error");
    }
    (StatusCode::OK, "ok")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_returns_ok() {
        let response = app(
            Registry::new(),
            IceConfig::from_env(),
            crate::db::Db::in_memory().expect("open in-memory db"),
            crate::passkeys::build_webauthn("localhost", "http://localhost:5173")
                .expect("build webauthn"),
            std::sync::Arc::new(crate::mail::DisabledMailer),
        )
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn healthz_returns_503_when_db_is_poisoned() {
        let db = crate::db::Db::in_memory().expect("open in-memory db");
        db.poison_for_test();
        let response = app(
            Registry::new(),
            IceConfig::from_env(),
            db,
            crate::passkeys::build_webauthn("localhost", "http://localhost:5173")
                .expect("build webauthn"),
            std::sync::Arc::new(crate::mail::DisabledMailer),
        )
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"db error");
    }
}
