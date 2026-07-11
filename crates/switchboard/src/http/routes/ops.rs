use crate::app::AppState;
use crate::http::wire::{ReadyResponse, StatusResponse};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

/// Liveness. Touches nothing, so it still answers while the gateway is saturated.
///
/// It sits outside every limiter (see [`crate::http::router`]) and does no I/O. A
/// liveness probe that can be starved by the thing it is probing turns one sick pod into
/// a rolling outage.
pub async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(StatusResponse::ok()))
}

pub async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    // A real query, not `db.is_some()`: the question is whether the ledger can be
    // written, not whether a pool struct was built at boot.
    let pg_ok = state.postgres_ok().await;
    let redis_ok = state.redis_ok().await;
    let provider_ok = state.registry.any_provider_healthy().await;
    let body = ReadyResponse::ready(pg_ok, redis_ok, provider_ok);

    if body.status == "ready" {
        (StatusCode::OK, Json(body))
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(body))
    }
}

pub async fn metrics() -> impl IntoResponse {
    if let Some(handle) = crate::telemetry::metrics::prometheus_handle() {
        (StatusCode::OK, handle.render())
    } else {
        (StatusCode::OK, "# metrics disabled\n".to_string())
    }
}
