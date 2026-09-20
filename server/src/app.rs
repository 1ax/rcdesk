use axum::routing::get;
use axum::Router;

use crate::db::Db;
use crate::ice::IceConfig;
use crate::registry::Registry;
use crate::ws::ws_handler;

/// Shared state handed to every `/ws` connection: the in-memory signaling
/// registry, the ICE server configuration used to fill `Registered`'s and
/// `Joined`'s `ice_servers` field, and the persistent device/owner store.
#[derive(Clone)]
pub struct AppState {
    pub registry: Registry,
    pub ice: IceConfig,
    pub db: Db,
}

pub fn app(registry: Registry, ice: IceConfig, db: Db) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_handler))
        .with_state(AppState { registry, ice, db })
}

async fn healthz() -> &'static str {
    "ok"
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
}
