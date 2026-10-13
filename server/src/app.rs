use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;

use crate::db::Db;
use crate::ice::IceConfig;
use crate::ip_limit::IpLimiter;
use crate::passkeys::SharedWebauthn;
use crate::registry::Registry;
use crate::ws::ws_handler;

/// Wrong-PIN guesses allowed in a burst per IP (slice 3.6a).
pub const PIN_BURST: u32 = 10;
pub const PIN_REFILL: Duration = Duration::from_secs(60);
/// New anonymous owners an IP may create in a burst (B2, audit S-H2).
pub const NEW_OWNER_BURST: u32 = 20;
pub const NEW_OWNER_REFILL: Duration = Duration::from_secs(60);
/// New devices an IP may register in a burst (B2, audit S-H2).
pub const NEW_DEVICE_BURST: u32 = 10;
pub const NEW_DEVICE_REFILL: Duration = Duration::from_secs(600);

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
    /// Per-IP limit on creating new owners (B2).
    pub owner_limiter: IpLimiter,
    /// Per-IP limit on registering new devices (B2).
    pub device_limiter: IpLimiter,
    /// WebAuthn relying party for passkeys (slice 3.7b).
    pub webauthn: SharedWebauthn,
}

pub fn app(registry: Registry, ice: IceConfig, db: Db, webauthn: SharedWebauthn) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_handler))
        .with_state(AppState {
            registry,
            ice,
            db,
            pin_limiter: IpLimiter::new(PIN_BURST, PIN_REFILL),
            owner_limiter: IpLimiter::new(NEW_OWNER_BURST, NEW_OWNER_REFILL),
            device_limiter: IpLimiter::new(NEW_DEVICE_BURST, NEW_DEVICE_REFILL),
            webauthn,
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
