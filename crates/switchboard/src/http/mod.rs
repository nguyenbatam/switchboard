pub mod error;
pub mod extract;
pub mod idempotency;
pub mod routes;
pub mod sse;
pub mod wire;

use crate::app::AppState;
use crate::http::error::ApiError;
use axum::{
    Router,
    error_handling::HandleErrorLayer,
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use std::time::Duration;
use tower::ServiceBuilder;
use tower::limit::ConcurrencyLimitLayer;
use tower::load_shed::LoadShedLayer;
use tower::timeout::TimeoutLayer;
use tower_http::{catch_panic::CatchPanicLayer, cors::CorsLayer, trace::TraceLayer};

/// Turns the two failures the `/v1` stack can produce into the two status codes
/// docs/15 §7 promises.
///
/// Without this, `ConcurrencyLimitLayer` answers a full semaphore with `Poll::Pending`
/// — it parks the request instead of refusing it. Request 257 waits, memory grows,
/// clients time out and retry, and the gateway never emits a single `503`. It does not
/// fail; it slows until it dies. `LoadShedLayer` is what converts that `Pending` into
/// an error, and this is what converts the error into an answer a client can act on.
///
/// A shed request is not a degraded p99. It is the mechanism that produces one: a hard
/// cap that refuses rather than queues sets queue wait to zero, so total latency stops
/// being a function of arrival rate. See docs/15 §3.
async fn handle_v1_error(err: tower::BoxError) -> axum::response::Response {
    if err.is::<tower::load_shed::error::Overloaded>() {
        return ApiError::overloaded(Some(SHED_RETRY_AFTER_MS)).into_response();
    }
    if err.is::<tower::timeout::error::Elapsed>() {
        return ApiError::DeadlineExceeded {
            message: "the gateway did not produce an answer within the request timeout".into(),
        }
        .into_response();
    }
    ApiError::Server {
        message: format!("unhandled internal error: {err}"),
    }
    .into_response()
}

/// How long a shed client should wait.
///
/// Short, and paired with full jitter on the client side: every caller received this
/// `503` at the same instant, so a fixed floor would bring them all back together and
/// shed them all again. See docs/15 §8.
const SHED_RETRY_AFTER_MS: u64 = 1_000;

pub fn router(state: AppState) -> Router {
    let request_timeout = Duration::from_millis(state.config.server.request_timeout_ms);

    let v1 = Router::new()
        .route("/chat/completions", post(routes::chat::chat_completions))
        .route("/completions", post(routes::completions::completions))
        .route("/models", get(routes::models::list_models))
        .route("/models/{model_id}", get(routes::models::get_model))
        .route("/embeddings", post(routes::embeddings::embeddings))
        .route("/documents", post(routes::documents::upload_document))
        .route(
            "/documents/{doc_id}",
            axum::routing::delete(routes::documents::delete_document),
        )
        .route("/search", post(routes::search::search))
        // Scope-gated to `keys` inside the handlers. A tenant that cannot rotate its own
        // credentials has to open a support ticket to respond to a leak.
        .route(
            "/keys",
            post(routes::keys::create_key).get(routes::keys::list_keys),
        )
        .route(
            "/keys/{key_id}",
            axum::routing::delete(routes::keys::revoke_key),
        )
        .route("/keys/{key_id}/rotate", post(routes::keys::rotate_key))
        .route("/usage", get(routes::usage::get_usage))
        .route("/usage/records", get(routes::usage::get_usage_records))
        .route(
            "/invocations/{request_id}",
            get(routes::invocations::get_invocations),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            extract::auth_middleware,
        ))
        .layer(
            ServiceBuilder::new()
                // Outermost, so the histogram sees the status a client sees — including
                // the `503`s produced below it. `outcome="shed"` is a label on a series
                // that would otherwise never be written.
                .layer(middleware::from_fn(crate::telemetry::inflight_layer))
                // Reads the limiter snapshot off the response, whether that response
                // came from a handler or from an `ApiError`.
                .layer(middleware::from_fn(extract::ratelimit_headers_layer))
                .layer(HandleErrorLayer::new(handle_v1_error))
                // Refuse, do not queue. This must wrap the concurrency limit: it works
                // by observing that the inner service is not ready.
                .layer(LoadShedLayer::new())
                .layer(ConcurrencyLimitLayer::new(
                    state.config.server.concurrency_limit,
                ))
                // Bounds the time to *produce a response*, not to stream its body — an
                // SSE handler returns as soon as headers are ready, so a long answer is
                // not killed by this.
                .layer(TimeoutLayer::new(request_timeout)),
        );

    // Deliberately outside every limiter above.
    //
    // These were previously `.merge`d before the `.layer(...)` call, which put them
    // inside the same `ConcurrencyLimitLayer` as `/v1`. When the gateway saturated, the
    // liveness probe queued with the traffic, Kubernetes killed the pod, its load landed
    // on the survivors, and they saturated in turn. A health check that can be starved by
    // the thing it is checking is an outage amplifier.
    let ops = Router::new()
        .route("/health", get(routes::ops::health))
        .route("/ready", get(routes::ops::ready))
        .route("/metrics", get(routes::ops::metrics));

    Router::new()
        .nest("/v1", v1)
        .merge(ops)
        .layer(
            ServiceBuilder::new()
                .layer(middleware::from_fn(crate::telemetry::request_id_layer))
                .layer(TraceLayer::new_for_http())
                .layer(CatchPanicLayer::new())
                .layer(CorsLayer::permissive()),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;

    /// The two codes a client's control loop is built on.
    ///
    /// `examples/bot-fleet` reduces its in-flight concurrency on `503` and abandons the
    /// request on anything else. If a shed ever answers `500` again, every bot in the
    /// fleet treats a capacity signal as a permanent bug and stops retrying.
    #[tokio::test]
    async fn a_shed_request_is_503_and_a_timeout_is_504() {
        let shed: tower::BoxError = Box::new(tower::load_shed::error::Overloaded::new());
        let response = handle_v1_error(shed).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "1");
        assert_eq!(response.headers()["retry-after-ms"], "1000");

        let elapsed: tower::BoxError = Box::new(tower::timeout::error::Elapsed::new());
        let response = handle_v1_error(elapsed).await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    /// Anything we did not anticipate is still a `500`. A `503` here would tell a client
    /// to retry a bug.
    #[tokio::test]
    async fn an_unrecognised_inner_error_stays_a_500() {
        let other: tower::BoxError = Box::<dyn std::error::Error + Send + Sync>::from("boom");
        let response = handle_v1_error(other).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
