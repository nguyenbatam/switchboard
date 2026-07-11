use metrics_exporter_prometheus::PrometheusBuilder;
use std::sync::OnceLock;

static HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();

pub fn init_metrics(enabled: bool) {
    if !enabled {
        return;
    }
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("prometheus recorder");
    let _ = HANDLE.set(handle);

    describe_metrics();
}

fn describe_metrics() {
    metrics::describe_counter!(
        "sb_requests_total",
        "Total HTTP requests by route, model, and status"
    );
    metrics::describe_histogram!(
        "sb_request_duration_seconds",
        "End-to-end request latency in seconds"
    );
    metrics::describe_histogram!("sb_ttft_seconds", "Time to first token in seconds");
    metrics::describe_histogram!(
        "sb_output_tokens_per_second",
        "Inter-token throughput (TPOT) of a streamed completion"
    );
    metrics::describe_counter!(
        "sb_tokens_total",
        "Token usage by model and kind (prompt|completion)"
    );
    metrics::describe_counter!(
        "sb_cost_nanodollars_total",
        "Billed cost in nanodollars by tenant-visible model alias"
    );
    metrics::describe_counter!(
        "sb_provider_errors_total",
        "Provider errors by provider and kind"
    );
    metrics::describe_gauge!(
        "sb_inflight_requests",
        "Requests currently in flight through /v1"
    );
    metrics::describe_gauge!(
        "sb_inflight_requests_peak",
        "High-water mark of concurrent /v1 requests since boot"
    );
    metrics::describe_gauge!(
        "sb_provider_inflight",
        "Upstream slots currently held, by provider"
    );
    metrics::describe_gauge!(
        "sb_provider_inflight_limit",
        "Configured providers.*.max_inflight. The denominator of the saturation alert."
    );
    metrics::describe_gauge!(
        "sb_circuit_breaker_state",
        "Circuit breaker state by provider (0=closed, 1=half_open, 2=open)"
    );
    metrics::describe_counter!(
        "sb_requests_shed_total",
        "Requests refused because the gateway or a provider was at capacity"
    );
    metrics::describe_counter!("sb_cache_hits_total", "Cache hits by kind");
    metrics::describe_counter!(
        "sb_ratelimit_denied_total",
        "Rate limit denials by dimension (rpm|tpm|ip|budget)"
    );
    metrics::describe_histogram!(
        "sb_agent_iterations",
        "Agent loop iterations per request by model"
    );
    metrics::describe_histogram!(
        "sb_retrieval_latency_seconds",
        "RAG retrieval latency by stage"
    );
    metrics::describe_counter!(
        "sb_usage_records_dropped_total",
        "Usage rows that could not be persisted. Money was spent and not written down."
    );
    metrics::describe_counter!(
        "sb_token_count_drift_total",
        "Requests whose token count was estimated, or disagreed with the upstream count by >2%"
    );
}

/// In-flight requests, and the high-water mark since boot.
///
/// The gauge answers "how loaded are we now"; the peak answers "how loaded did we ever
/// get", which is the only one you can read after the incident. Prometheus scrapes every
/// 15s and a saturation spike lasts 400ms, so a gauge alone will not see it.
///
/// Kept as an atomic rather than derived from the histogram because concurrency is not a
/// latency percentile and pretending otherwise is how people end up quoting p99 of a
/// number that has no distribution.
static INFLIGHT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static INFLIGHT_PEAK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn inflight_enter() -> u64 {
    use std::sync::atomic::Ordering::Relaxed;
    let n = INFLIGHT.fetch_add(1, Relaxed) + 1;
    INFLIGHT_PEAK.fetch_max(n, Relaxed);
    metrics::gauge!("sb_inflight_requests").set(n as f64);
    metrics::gauge!("sb_inflight_requests_peak").set(INFLIGHT_PEAK.load(Relaxed) as f64);
    n
}

pub fn inflight_exit() {
    use std::sync::atomic::Ordering::Relaxed;
    let n = INFLIGHT.fetch_sub(1, Relaxed).saturating_sub(1);
    metrics::gauge!("sb_inflight_requests").set(n as f64);
}

pub fn inflight_peak() -> u64 {
    INFLIGHT_PEAK.load(std::sync::atomic::Ordering::Relaxed)
}

/// `outcome` is `served | shed | timeout | error`, and it is the label that stops this
/// histogram from lying. A shed request returns `503` in about a millisecond; bucketed
/// with real traffic it drags p99 *down* as the gateway gets sicker. See docs/15 §4.
pub fn record_request_duration(route: &str, outcome: &str, secs: f64) {
    metrics::histogram!(
        "sb_request_duration_seconds",
        "route" => route.to_string(),
        "outcome" => outcome.to_string()
    )
    .record(secs);
}

pub fn prometheus_handle() -> Option<&'static metrics_exporter_prometheus::PrometheusHandle> {
    HANDLE.get()
}

pub fn record_request(route: &str, model: &str, status: &str) {
    metrics::counter!("sb_requests_total", "route" => route.to_string(), "model" => model.to_string(), "status" => status.to_string()).increment(1);
}

pub fn record_tokens(model: &str, kind: &str, count: u64) {
    metrics::counter!("sb_tokens_total", "model" => model.to_string(), "kind" => kind.to_string())
        .increment(count);
}

pub fn record_cost(model: &str, cost_nano: i64) {
    if cost_nano <= 0 {
        return;
    }
    metrics::counter!("sb_cost_nanodollars_total", "model" => model.to_string())
        .increment(cost_nano as u64);
}

/// Counted separately from the latency histogram, and read *beside* it.
///
/// A shed request returns `503` in about a millisecond. Bucketed with served traffic it
/// drags p99 *down* as the gateway saturates: the graph improves, the alert falls silent,
/// and the outage continues. The `outcome` label keeps them apart; this counter is what
/// tells you the served-p99 you are looking at describes a shrinking fraction of traffic.
pub fn record_shed(route: &str) {
    metrics::counter!("sb_requests_shed_total", "route" => route.to_string()).increment(1);
}

pub fn record_cache_hit(kind: &str) {
    metrics::counter!("sb_cache_hits_total", "kind" => kind.to_string()).increment(1);
}

pub fn record_provider_error(provider: &str, kind: &str) {
    metrics::counter!("sb_provider_errors_total", "provider" => provider.to_string(), "kind" => kind.to_string()).increment(1);
}

pub fn record_retrieval(stage: &str, secs: f64) {
    metrics::histogram!("sb_retrieval_latency_seconds", "stage" => stage.to_string()).record(secs);
}

pub fn record_ttft(model: &str, secs: f64) {
    metrics::histogram!("sb_ttft_seconds", "model" => model.to_string()).record(secs);
}

/// TPOT. Plotted against TTFT, this separates "slow to start" from "slow to write".
pub fn record_output_tps(model: &str, tps: f64) {
    metrics::histogram!("sb_output_tokens_per_second", "model" => model.to_string()).record(tps);
}

/// Rises with a provider permit and falls when it is released.
///
/// A gauge rather than a counter: the question it answers is "are we at this provider's
/// cap right now", which is what decides whether a `503` is capacity or a bug.
pub fn provider_inflight_enter(provider: &str) {
    metrics::gauge!("sb_provider_inflight", "provider" => provider.to_string()).increment(1.0);
}

pub fn provider_inflight_exit(provider: &str) {
    metrics::gauge!("sb_provider_inflight", "provider" => provider.to_string()).decrement(1.0);
}

/// Published once at boot.
///
/// Without it `sb_provider_inflight` has no denominator, and a saturation alert has to be
/// written as `> 0` — which fires whenever anybody is using the gateway. An alert that
/// cannot distinguish "busy" from "full" is one people learn to ignore.
pub fn set_provider_inflight_limit(provider: &str, permits: usize) {
    metrics::gauge!("sb_provider_inflight_limit", "provider" => provider.to_string())
        .set(permits as f64);
}

pub fn record_ratelimit_denied(dimension: &str) {
    metrics::counter!("sb_ratelimit_denied_total", "dimension" => dimension.to_string())
        .increment(1);
}

pub fn record_agent_iterations(model: &str, iterations: f64) {
    metrics::histogram!("sb_agent_iterations", "model" => model.to_string()).record(iterations);
}

pub fn record_usage_dropped() {
    record_usage_dropped_n(1);
}

/// A whole batch failed to persist. Counting it as one would understate the loss by up to
/// the batch size.
pub fn record_usage_dropped_n(n: u64) {
    metrics::counter!("sb_usage_records_dropped_total").increment(n);
}

pub fn record_token_drift(model: &str) {
    metrics::counter!("sb_token_count_drift_total", "model" => model.to_string()).increment(1);
}

pub fn set_circuit_breaker_state(provider: &str, state: crate::reliability::BreakerState) {
    let value = match state {
        crate::reliability::BreakerState::Closed => 0.0,
        crate::reliability::BreakerState::HalfOpen => 1.0,
        crate::reliability::BreakerState::Open => 2.0,
    };
    metrics::gauge!("sb_circuit_breaker_state", "provider" => provider.to_string()).set(value);
}
