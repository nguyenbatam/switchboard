pub mod logs;
pub mod metrics;
pub mod traces;

pub use logs::init_tracing;
pub use metrics::init_metrics;
pub use traces::request_id_layer;

/// Counts in-flight `/v1` requests and records how long each took.
///
/// Two things live here because they are measured at the same instant and nowhere else:
/// the concurrency high-water mark, and `sb_request_duration_seconds` — which
/// [docs/13 §2.5] observes has existed as a `describe_histogram!` with no call sites, so
/// the panel reads "no traffic" rather than "no instrumentation".
///
/// It wraps `/v1` only. `/health` must not be counted as load, or a Kubernetes probe
/// becomes traffic.
pub async fn inflight_layer(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let started = std::time::Instant::now();
    metrics::inflight_enter();
    let response = next.run(req).await;
    metrics::inflight_exit();

    // An `ApiError` labels itself, because a status code cannot. Two different `503`s
    // exist: the gateway refusing because it is full, and a provider that is down. Only the
    // first is a shed, and only the first belongs in the shed-rate SLO — otherwise an
    // OpenAI outage pages whoever is on call for gateway saturation, which is exactly what
    // docs/15 §1's "alert on overhead, dashboard the provider" exists to prevent.
    //
    // The fallback covers responses that never passed through `ApiError`: the `LoadShedLayer`
    // rejection that `handle_v1_error` turns into a `503`, and every `2xx`.
    let outcome = response
        .extensions()
        .get::<crate::http::error::Outcome>()
        .map(|o| o.0)
        .unwrap_or_else(|| match response.status().as_u16() {
            200..=299 => "served",
            503 => "shed",
            504 => "timeout",
            _ => "error",
        });

    if outcome == "shed" {
        metrics::record_shed("v1");
    }
    metrics::record_request_duration("v1", outcome, started.elapsed().as_secs_f64());
    response
}
