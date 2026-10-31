//! D85: роутер-будильник. Роутер (MikroTik/OpenWrt) не держит WebSocket, а
//! раз в ~10 с опрашивает `GET /wake/poll` со своим токеном и получает MAC,
//! которые надо разбудить (`/tool wol`, `etherwake`).

use std::net::SocketAddr;
use std::time::Instant;

use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::app::AppState;
use crate::devices::hash_secret;
use crate::ws::{client_ip, notify_owner};

/// Токен из `Authorization: Bearer <token>`.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

pub async fn wake_poll(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = client_ip(&headers, peer);
    if !state.router_poll_limiter.check(ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    state.router_poll_limiter.spend(ip);

    let Some(token) = bearer_token(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let (row, owner_id) = match state.db.wake_device_by_token_hash(&hash_secret(token)) {
        Ok(Some(found)) => found,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(err) => {
            tracing::error!(?err, "wake poll: db lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let now = Instant::now();
    if state.registry.router_polled(&row.wake_id, now) {
        notify_owner(&state.db, &state.registry, &owner_id);
    }
    let macs = state.registry.drain_router_wakes(&row.wake_id, now);
    if !macs.is_empty() {
        tracing::info!(wake_id = %row.wake_id, count = macs.len(), "router wake delivered");
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        macs.join(","),
    )
        .into_response()
}
