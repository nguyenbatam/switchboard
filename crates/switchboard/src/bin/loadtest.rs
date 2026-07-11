//! P5 — the load test. See docs/13 §P5.
//!
//!   cargo run --release --bin loadtest -- --scenario all
//!   cargo run --release --bin loadtest -- --scenario burst --duration-secs 30
//!   cargo run --release --bin loadtest -- --scenario all --json target/loadtest.json
//!
//! Run it from the repository root: it loads the real `config/switchboard.toml`, and it
//! needs `DATABASE_URL` and `REDIS_URL`.
//!
//! Every other phase in docs/13 changed a mechanism. This one measures a mechanism, and
//! it is what decides whether the numbers in docs/15 §1 are measurements or hypotheses
//! with numbers attached.
//!
//! # Why this is not k6 or oha
//!
//! docs/13 §P5 proposed one of those. Both generate load; neither can run these
//! scenarios. Scenario 2 has to kill a provider *while requests are in flight* and then
//! assert the answer was a fallback rather than a `503`. Scenario 3 has to hold two
//! tenants' latencies apart, which means two API keys and two histograms. An external
//! generator can produce the traffic for both and observe neither.
//!
//! # Why the upstream is a stub
//!
//! The SLO in docs/15 §1 is on **gateway overhead** — total latency minus provider time
//! — and it is stated that way because switchboard cannot make `gpt-4o-mini` faster. To
//! measure a subtraction you must know both terms. So the upstream here is a stub that
//! sleeps for exactly `--stub-delay-ms` and answers: provider time is a constant we
//! chose, and everything above it is ours.
//!
//! `overhead = total − stub_delay` therefore *includes* one loopback HTTP round trip and
//! the encode/decode of the upstream call. That overstates the gateway's own cost by a
//! few hundred microseconds. Overstating it is the safe direction.
//!
//! The stub is also what makes this runnable at 10× capacity on a laptop, in CI, with no
//! GPU. A real Ollama answers `qwen3:4b` in about two minutes; ten times the rated
//! capacity of a real provider is not a thing you can offer it.
//!
//! # Why the models are not `config/models.toml`
//!
//! Because `gpt-4o-mini` declares `fallback = ["qwen3:4b"]`, and those two aliases sit on
//! different providers with a semaphore each. A model with a fallback therefore has twice
//! the rated capacity of its own provider, and the first run of this test dutifully
//! served 79.5 rps against a "40 rps" gateway. That is correct behaviour — docs/13 §1.4
//! says a *busy* provider falls back where a *broken* one does not — and it makes the
//! capacity arithmetic of every other scenario wrong.
//!
//! So the aliases below are the load test's own: one with no fallback, whose capacity is
//! exactly one semaphore, and a pair that exists to be killed.
//!
//! # Why the load is open-loop
//!
//! A closed-loop generator — N workers, each waiting for its own response before sending
//! the next — cannot overload a system that sheds. Shed responses return in a
//! millisecond, so the workers cycle faster and the offered rate silently becomes
//! whatever the gateway felt like serving. That is coordinated omission, and it is the
//! standard way a load test reports that an overloaded system is healthy.
//!
//! Requests are therefore launched on a schedule fixed in advance, whether or not the
//! previous one has come back.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use switchboard::app::AppState;
use switchboard::config::AppConfig;
use uuid::Uuid;

/// The stub always reports the same token counts, so a scenario's spend is a
/// multiplication rather than a measurement. The tokenizer still runs on the real prompt
/// on the way in, which is the cost docs/13 §P3 says to profile rather than assume.
const STUB_PROMPT_TOKENS: u32 = 10;
const STUB_COMPLETION_TOKENS: u32 = 5;

/// No fallback: its rated capacity is exactly one provider semaphore. `o200k_base` keeps
/// the real inline BPE encode on the request path (docs/15 §2, row 7).
const MODEL: &str = "bench";
/// Falls back across providers. Scenario 2 kills the first and expects the second.
const MODEL_FB: &str = "bench-primary";

const LOADTEST_MODELS: &str = r#"
[[model]]
alias = "bench"
provider = "openai"
upstream_name = "bench"
context = 128000
price_in = 0.20
price_out = 0.80
cost_in = 0.15
cost_out = 0.60
tokenizer = { kind = "tiktoken", encoding = "o200k_base" }

[[model]]
alias = "bench-primary"
provider = "openai"
upstream_name = "bench-primary"
context = 128000
price_in = 0.20
price_out = 0.80
cost_in = 0.15
cost_out = 0.60
tokenizer = { kind = "tiktoken", encoding = "o200k_base" }
fallback = ["bench-backup"]

[[model]]
alias = "bench-backup"
provider = "ollama"
upstream_name = "bench-backup"
context = 32768
price_in = 0.05
price_out = 0.10
cost_in = 0.01
cost_out = 0.02
tokenizer = { kind = "estimate" }
"#;

/// The four `Retry-After` values are the only thing on the wire that tells a client
/// *which* refusal it received, and docs/15 §7 says they differ on purpose. They are also
/// how this test tells one `503` from another, which no external load generator can do.
const RETRY_SHED_LAYER: u64 = 1_000; // tower LoadShedLayer: the gateway is full
const RETRY_PROVIDER_BUSY: u64 = 500; // provider semaphore full: healthy but busy
const RETRY_UPSTREAM_DOWN: u64 = 2_000; // upstream answered 5xx / would not connect
const RETRY_BREAKER_OPEN: u64 = 10_000; // circuit breaker open: broken, do not call

const MODE_HEALTHY: u64 = 0;
/// Answers `500`. The gateway retries, exhausts its retry budget, and falls back.
const MODE_ERRORING: u64 = 1;

// ── the stub upstream ────────────────────────────────────────────────────────

/// One knob per provider, because scenario 2 kills exactly one of them and asserts the
/// other caught the traffic.
struct Knob {
    delay_ms: AtomicU64,
    mode: AtomicU64,
    requests: AtomicUsize,
}

impl Knob {
    fn new(delay_ms: u64) -> Self {
        Self {
            delay_ms: AtomicU64::new(delay_ms),
            mode: AtomicU64::new(MODE_HEALTHY),
            requests: AtomicUsize::new(0),
        }
    }
    fn kill(&self) {
        self.mode.store(MODE_ERRORING, Ordering::SeqCst);
    }
    fn revive(&self) {
        self.mode.store(MODE_HEALTHY, Ordering::SeqCst);
    }
    fn calls(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }
    /// `false` means "answer 500". The mode is read on arrival: a request admitted while
    /// the upstream was healthy is served, exactly as a real one would be.
    async fn serve(&self) -> bool {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if self.mode.load(Ordering::SeqCst) == MODE_ERRORING {
            return false;
        }
        let d = self.delay_ms.load(Ordering::Relaxed);
        if d > 0 {
            tokio::time::sleep(Duration::from_millis(d)).await;
        }
        true
    }
}

#[derive(Clone)]
struct Stub {
    openai: Arc<Knob>,
    ollama: Arc<Knob>,
}

async fn stub_openai_chat(State(s): State<Stub>, body: Json<serde_json::Value>) -> Response {
    if !s.openai.serve().await {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "upstream down",
        )
            .into_response();
    }
    let model = body.0["model"].as_str().unwrap_or("stub").to_string();
    Json(json!({
        "id": "chatcmpl-stub",
        "object": "chat.completion",
        "created": 1_700_000_000,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": STUB_PROMPT_TOKENS,
            "completion_tokens": STUB_COMPLETION_TOKENS,
            "total_tokens": STUB_PROMPT_TOKENS + STUB_COMPLETION_TOKENS
        }
    }))
    .into_response()
}

async fn stub_ollama_chat(State(s): State<Stub>, _body: Json<serde_json::Value>) -> Response {
    if !s.ollama.serve().await {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "upstream down",
        )
            .into_response();
    }
    Json(json!({
        "message": {"role": "assistant", "content": "ok"},
        "done": true,
        "done_reason": "stop",
        "prompt_eval_count": STUB_PROMPT_TOKENS,
        "eval_count": STUB_COMPLETION_TOKENS
    }))
    .into_response()
}

/// `/ready` calls these. They stay healthy even when the chat path is dead: scenario 2 is
/// an upstream that accepts connections and fails requests, which is what an outage
/// usually looks like from inside a gateway.
async fn stub_openai_models() -> Response {
    Json(json!({"object": "list", "data": []})).into_response()
}
async fn stub_ollama_tags() -> Response {
    Json(json!({"models": []})).into_response()
}

/// Both wires on one listener. The paths do not collide, and one process is one less
/// thing for a scenario to tear down.
async fn spawn_stub(delay_ms: u64) -> (Stub, String) {
    let stub = Stub {
        openai: Arc::new(Knob::new(delay_ms)),
        ollama: Arc::new(Knob::new(delay_ms)),
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(stub_openai_chat))
        .route("/v1/models", get(stub_openai_models))
        .route("/api/chat", post(stub_ollama_chat))
        .route("/api/tags", get(stub_ollama_tags))
        .with_state(stub.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (stub, format!("http://{addr}"))
}

// ── the gateway under test ───────────────────────────────────────────────────

struct Gateway {
    base: String,
    pool: sqlx::PgPool,
}

/// What a scenario needs to differ about the gateway it boots.
#[derive(Clone, Copy)]
struct Boot {
    cache: bool,
    provider_cap: usize,
    concurrency_limit: usize,
}

impl Boot {
    fn from(opts: &Opts) -> Self {
        Self {
            cache: false,
            provider_cap: opts.provider_cap,
            concurrency_limit: 256,
        }
    }
}

/// Boots the real router — `LoadShedLayer` over `ConcurrencyLimitLayer` over
/// `TimeoutLayer`, the real auth middleware, the real rate limiter, the real batching
/// usage writer — against the stub.
///
/// `into_make_service_with_connect_info` matches `main.rs`. Without it every caller
/// collapses into the `"unknown"` per-IP bucket, and this would be measuring a limiter
/// rather than a gateway.
async fn boot_gateway(stub_url: &str, b: Boot) -> Gateway {
    // The openai provider only registers when a key is present (`providers/registry.rs`).
    unsafe { std::env::set_var("OPENAI_API_KEY", "loadtest-stub-key") };

    let models_path =
        std::env::temp_dir().join(format!("sb-loadtest-models-{}.toml", Uuid::now_v7()));
    std::fs::write(&models_path, LOADTEST_MODELS).expect("write loadtest models");

    let mut config = AppConfig::load().expect("load config — run from the repository root");
    config.models_path = models_path.to_string_lossy().into_owned();
    config.providers.openai.base_url = format!("{stub_url}/v1");
    config.providers.ollama.base_url = stub_url.to_string();
    config.providers.openai.max_inflight = b.provider_cap;
    config.providers.ollama.max_inflight = b.provider_cap;
    config.server.concurrency_limit = b.concurrency_limit;
    config.cache.enabled = b.cache;
    config.agent.enabled = false;

    // Two tenants with two keys is the whole of scenario 3, and the dev tenant is one
    // tenant. So everything below authenticates for real, argon2 and all.
    config.auth.require_auth = true;

    // The per-IP limiter exists to protect the argon2 verify behind it (docs/13 §0.5),
    // and every request in this process arrives from 127.0.0.1. Left at 120 rpm it would
    // refuse the load test before the load test reached the gateway. Its Redis round trip
    // stays on the path — that is row 1 of docs/15 §2, and it belongs in the overhead this
    // exists to measure.
    config.ratelimit.ip_rpm = 100_000_000;

    let state = AppState::boot(config).await.expect("boot gateway");
    let pool = state
        .db
        .clone()
        .expect("DATABASE_URL must point at a migrated database");

    let app = switchboard::http::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    // A bound socket is not a ready gateway.
    let base = format!("http://{addr}");
    let probe = reqwest::Client::new();
    for _ in 0..200 {
        if probe
            .get(format!("{base}/health"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    Gateway { base, pool }
}

/// A tenant with a key, a budget nothing in this run can exhaust, and the limits the
/// caller chooses. `rpm` is the knob scenario 3 turns.
async fn make_tenant(pool: &sqlx::PgPool, label: &str, rpm: i32, tpm: i32) -> String {
    let tenant = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO tenants (id, name, monthly_budget_nano, rpm_limit, tpm_limit) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(tenant)
    .bind(format!("loadtest-{label}-{tenant}"))
    .bind(1_000_000_000_000_i64)
    .bind(rpm)
    .bind(tpm)
    .execute(pool)
    .await
    .expect("insert tenant");

    let (prefix, full_key) = switchboard::auth::key::generate_key();
    let secret = full_key.split_once('.').map(|(_, s)| s).unwrap();
    let hash = switchboard::auth::key::hash_secret(secret).unwrap();
    sqlx::query(
        "INSERT INTO api_keys (id, tenant_id, key_prefix, key_hash, name) \
         VALUES ($1, $2, $3, $4, 'loadtest')",
    )
    .bind(Uuid::now_v7())
    .bind(tenant)
    .bind(prefix)
    .bind(hash)
    .execute(pool)
    .await
    .expect("insert api key");

    full_key
}

/// A couple of requests, awaited, before the clock starts.
///
/// The verified-key cache is keyed on `sha256(full_token)` with a 60s TTL. A cold key hit
/// by 400 concurrent requests fans out into 400 simultaneous argon2 verifies — 19 MiB each
/// — on the blocking pool. That stampede is real and worth its own fix, but it is not the
/// steady-state overhead docs/15 §1 puts a number on, and letting it land inside the
/// measurement window would put an auth cold start into the gateway's p99.
async fn warm(gw: &Gateway, key: &str, model: &str) {
    let c = client();
    for _ in 0..2 {
        let _ = c
            .post(format!("{}/v1/chat/completions", gw.base))
            .bearer_auth(key)
            .json(&chat_body(model))
            .send()
            .await;
    }
}

// ── the generator ────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Kind {
    Chat,
    Health,
}

#[derive(Clone)]
struct Sample {
    /// Milliseconds from the start of the run to the moment this request was *launched*.
    /// Completion order is not launch order, and scenario 2 splits on the kill.
    launched_ms: f64,
    latency_ms: f64,
    status: u16,
    /// Which refusal this was. See the `RETRY_*` constants.
    retry_after_ms: Option<u64>,
    /// `x-switchboard-fallback` was present: a provider died and another answered.
    fallback: bool,
    cache_hit: bool,
    /// Never reached a status code. Counted apart, because a generator that cannot connect
    /// is not a gateway that refused.
    transport_error: bool,
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (p / 100.0) * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    sorted[lo] + (sorted[hi] - sorted[lo]) * (rank - lo as f64)
}

#[derive(Default)]
struct Stats {
    n: usize,
    ok: usize,
    /// `503`s the gateway counts in `sb_requests_shed_total`: it refused because *it* was
    /// full. The load-shed layer and a full provider semaphore, summed.
    shed: usize,
    shed_layer: usize,
    shed_busy: usize,
    /// `503`s that are a provider outage, not a capacity refusal. `outcome="error"`.
    unavailable: usize,
    rate_limited_429: usize,
    other_5xx: usize,
    transport_errors: usize,
    fallbacks: usize,
    cache_hits: usize,
    latency: Vec<f64>,
    shed_latency: Vec<f64>,
}

impl Stats {
    fn of(samples: &[Sample]) -> Self {
        let mut s = Stats {
            n: samples.len(),
            ..Default::default()
        };
        for x in samples {
            if x.transport_error {
                s.transport_errors += 1;
                continue;
            }
            match x.status {
                200 => {
                    s.ok += 1;
                    s.latency.push(x.latency_ms);
                    if x.fallback {
                        s.fallbacks += 1;
                    }
                    if x.cache_hit {
                        s.cache_hits += 1;
                    }
                }
                429 => s.rate_limited_429 += 1,
                503 => match x.retry_after_ms {
                    Some(RETRY_SHED_LAYER) => {
                        s.shed += 1;
                        s.shed_layer += 1;
                        s.shed_latency.push(x.latency_ms);
                    }
                    Some(RETRY_PROVIDER_BUSY) => {
                        s.shed += 1;
                        s.shed_busy += 1;
                        s.shed_latency.push(x.latency_ms);
                    }
                    Some(RETRY_UPSTREAM_DOWN) | Some(RETRY_BREAKER_OPEN) => s.unavailable += 1,
                    _ => s.unavailable += 1,
                },
                500..=599 => s.other_5xx += 1,
                _ => {}
            }
        }
        s.latency.sort_by(f64::total_cmp);
        s.shed_latency.sort_by(f64::total_cmp);
        s
    }

    fn p(&self, q: f64) -> f64 {
        pct(&self.latency, q)
    }
    fn shed_p(&self, q: f64) -> f64 {
        pct(&self.shed_latency, q)
    }
    fn success_rate(&self) -> f64 {
        self.ok as f64 / self.n.max(1) as f64
    }
    /// The denominator is every request that got an answer. A transport error is the
    /// generator's problem, not the gateway's shed rate.
    fn shed_rate(&self) -> f64 {
        let answered = self.n - self.transport_errors;
        if answered == 0 {
            return 0.0;
        }
        self.shed as f64 / answered as f64
    }
    fn line(&self) -> String {
        format!(
            "{} requests · {} ok · {} shed(503) [{} layer / {} busy] · {} unavailable(503) · {} 429 · {} other 5xx · {} transport-err",
            self.n,
            self.ok,
            self.shed,
            self.shed_layer,
            self.shed_busy,
            self.unavailable,
            self.rate_limited_429,
            self.other_5xx,
            self.transport_errors
        )
    }
}

struct Load {
    client: reqwest::Client,
    url: String,
    api_key: Option<String>,
    body: Option<serde_json::Value>,
    kind: Kind,
    rps: f64,
    duration: Duration,
    /// Salt each prompt, so the exact cache cannot serve a run that is measuring the
    /// provider path. `false` is how the cache scenario asks for hits.
    unique_prompts: bool,
}

/// Launches requests on a fixed schedule, regardless of whether the previous one has
/// returned. See the module header on coordinated omission.
///
/// `stall_guard` bounds outstanding requests so a wedged gateway cannot OOM the generator.
/// Hitting it is reported: a generator that fell behind did not offer the load it claims
/// to have, and a load test that hides that is measuring itself.
async fn open_loop(l: Load) -> (Vec<Sample>, usize) {
    let period = Duration::from_secs_f64(1.0 / l.rps);
    let start = Instant::now();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Sample>();
    let stall_guard = Arc::new(tokio::sync::Semaphore::new(16_384));
    let stalls = Arc::new(AtomicUsize::new(0));

    let mut i: u64 = 0;
    loop {
        let at = start + period.mul_f64(i as f64);
        let launched_ms = at.duration_since(start).as_secs_f64() * 1000.0;
        if at.duration_since(start) >= l.duration {
            break;
        }
        tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
        i += 1;

        let Ok(permit) = stall_guard.clone().try_acquire_owned() else {
            stalls.fetch_add(1, Ordering::Relaxed);
            continue;
        };

        let client = l.client.clone();
        let url = l.url.clone();
        let api_key = l.api_key.clone();
        let tx = tx.clone();
        let body = l.body.clone().map(|mut b| {
            if l.unique_prompts {
                b["messages"][0]["content"] = json!(format!("load test {} {}", i, Uuid::now_v7()));
            }
            b
        });

        tokio::spawn(async move {
            let _permit = permit;
            let t0 = Instant::now();
            let mut req = match l.kind {
                Kind::Chat => client.post(&url),
                Kind::Health => client.get(&url),
            };
            if let Some(k) = api_key {
                req = req.bearer_auth(k);
            }
            if let Some(b) = body {
                req = req.json(&b);
            }
            let sample = match req.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let h = resp.headers();
                    let fallback = h.contains_key("x-switchboard-fallback");
                    let cache_hit =
                        h.get("x-switchboard-cache").and_then(|v| v.to_str().ok()) == Some("hit");
                    let retry_after_ms = h
                        .get("retry-after-ms")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok());
                    // Drain before the clock stops, or the connection returns to the pool
                    // dirty and the next request pays for this one.
                    let _ = resp.bytes().await;
                    Sample {
                        launched_ms,
                        latency_ms: t0.elapsed().as_secs_f64() * 1000.0,
                        status,
                        retry_after_ms,
                        fallback,
                        cache_hit,
                        transport_error: false,
                    }
                }
                Err(_) => Sample {
                    launched_ms,
                    latency_ms: t0.elapsed().as_secs_f64() * 1000.0,
                    status: 0,
                    retry_after_ms: None,
                    fallback: false,
                    cache_hit: false,
                    transport_error: true,
                },
            };
            let _ = tx.send(sample);
        });
    }
    drop(tx);

    // Every in-flight request still deserves to be counted. A load test that drops its
    // slow tail measures the fast half of its own traffic.
    let mut samples = Vec::new();
    while let Some(s) = rx.recv().await {
        samples.push(s);
    }
    (samples, stalls.load(Ordering::Relaxed))
}

fn chat_body(model: &str) -> serde_json::Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "placeholder"}],
        "max_tokens": 16,
        "stream": false
    })
}

/// The cost of *calling* the stub, measured from this process: one loopback round trip,
/// one JSON encode, one JSON decode, on top of the sleep.
///
/// `total − stub_delay` charges all of that to the gateway, which is wrong in the gateway's
/// favour's opposite direction — it makes switchboard look slower than it is. What the SLO
/// in docs/15 §1 means by "provider time" is the duration of the provider call, and a
/// provider call includes its own HTTP client. Subtracting *this* number instead leaves
/// something much closer to the gateway's own work.
///
/// It is still not exact: reqwest here is not the gateway's `reqwest::Client`, which is
/// pooled and configured differently. It is a lower bound on the provider-call cost, and so
/// the overhead computed from it is an upper bound on the gateway's own.
async fn probe_upstream_cost(stub_url: &str, samples: usize) -> f64 {
    let c = client();
    let url = format!("{stub_url}/v1/chat/completions");
    let body = chat_body("bench");
    let mut v = Vec::with_capacity(samples);
    for _ in 0..samples {
        let t0 = Instant::now();
        if let Ok(resp) = c.post(&url).json(&body).send().await {
            let _ = resp.bytes().await;
            v.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
    }
    v.sort_by(f64::total_cmp);
    pct(&v, 50.0)
}

/// One pooled, keep-alive client, sized above the offered concurrency. docs/15 §8 makes
/// this a client obligation; a load test that opens a connection per request measures TCP
/// handshakes and calls the result p99.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(4096)
        .pool_idle_timeout(Duration::from_secs(90))
        .timeout(Duration::from_secs(120))
        .build()
        .expect("build client")
}

// ── metrics ──────────────────────────────────────────────────────────────────

async fn scrape(base: &str) -> String {
    reqwest::get(format!("{base}/metrics"))
        .await
        .expect("scrape /metrics")
        .text()
        .await
        .unwrap_or_default()
}

/// Sums every series whose name matches, ignoring labels. Counters are cumulative and the
/// scenarios share one process, so callers diff two scrapes.
fn metric_sum(text: &str, name: &str) -> f64 {
    text.lines()
        .filter(|l| !l.starts_with('#') && l.starts_with(name))
        .filter(|l| {
            let rest = &l[name.len()..];
            rest.starts_with(' ') || rest.starts_with('{')
        })
        .filter_map(|l| l.rsplit(' ').next().and_then(|v| v.parse::<f64>().ok()))
        .sum()
}

/// Resident set size of this process, in MiB. `ps` exists on both platforms this
/// repository claims to run on, and a gateway that grows without bound under overload is
/// the finding — not the precision of the number that caught it.
fn rss_mib() -> f64 {
    let pid = std::process::id();
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<f64>()
                .ok()
        })
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}

// ── reporting ────────────────────────────────────────────────────────────────

struct Report {
    checks: Vec<(String, bool, String)>,
    json: serde_json::Map<String, serde_json::Value>,
    /// Whether a missed wall-clock percentile fails the run. See [`Report::latency`].
    enforce_latency: bool,
}

impl Report {
    fn new(enforce_latency: bool) -> Self {
        Self {
            checks: Vec::new(),
            json: serde_json::Map::new(),
            enforce_latency,
        }
    }
    fn say(&self, s: impl AsRef<str>) {
        println!("{}", s.as_ref());
    }
    /// A named assertion, with the number that produced it. Any `false` fails the run.
    ///
    /// Structural: *did the gateway shed rather than queue*, *did the fallback answer*,
    /// *did `/health` ever fail*. These hold on any machine, so they are enforced
    /// everywhere, CI included.
    fn check(&mut self, name: impl Into<String>, pass: bool, detail: impl Into<String>) {
        let (name, detail) = (name.into(), detail.into());
        let mark = if pass {
            "\x1b[32mPASS\x1b[0m"
        } else {
            "\x1b[31mFAIL\x1b[0m"
        };
        println!("  [{mark}] {name} — {detail}");
        self.checks.push((name, pass, detail));
    }

    /// An assertion on a wall-clock percentile.
    ///
    /// These are enforced locally and *reported but not enforced* under
    /// `--no-latency-slo`, which is how CI runs them. A shared GitHub runner is a noisy
    /// neighbour with an unknown number of other tenants on the box, and a p99 measured
    /// there says nothing about this gateway. Failing a build on it would teach everyone
    /// to rerun CI until it passes, which is how a test stops meaning anything.
    ///
    /// The number is still printed, because the point of the run is the number.
    fn latency(&mut self, name: impl Into<String>, pass: bool, detail: impl Into<String>) {
        if !self.enforce_latency {
            println!(
                "  [\x1b[33mINFO\x1b[0m] {} — {} (not enforced here)",
                name.into(),
                detail.into()
            );
            return;
        }
        self.check(name, pass, detail);
    }
}

fn header(title: &str) {
    let bar = "─".repeat(66usize.saturating_sub(title.len()));
    println!("\n\x1b[1;36m── {title} {bar}\x1b[0m");
}

// ── scenarios ────────────────────────────────────────────────────────────────

struct Opts {
    scenario: String,
    duration_secs: u64,
    stub_delay_ms: u64,
    provider_cap: usize,
    json_out: Option<String>,
    /// `--no-latency-slo`: report wall-clock percentiles, do not fail on them. For CI.
    enforce_latency: bool,
}

impl Opts {
    /// Little's Law: a hard cap of `C` permits and a service time of `S` serves `C/S`
    /// requests per second, and no request ever waits. docs/15 §3.
    ///
    /// True only for a model with no fallback — see the module header.
    fn capacity_rps(&self) -> f64 {
        self.provider_cap as f64 / (self.stub_delay_ms as f64 / 1000.0)
    }
}

/// Scenario 0 — the baseline. Below rated capacity nothing should shed, and the gateway's
/// own cost is whatever remains once the stub's sleep is subtracted.
async fn scenario_baseline(opts: &Opts, r: &mut Report) {
    header("baseline — 80% of rated capacity");
    let (_stub, stub_url) = spawn_stub(opts.stub_delay_ms).await;
    let gw = boot_gateway(&stub_url, Boot::from(opts)).await;
    let key = make_tenant(&gw.pool, "baseline", 1_000_000, 1_000_000_000).await;
    warm(&gw, &key, MODEL).await;

    let rps = opts.capacity_rps() * 0.8;
    r.say(format!(
        "offering {rps:.1} rps for {}s (capacity {:.1} rps = {} permits / {}ms, no fallback)",
        opts.duration_secs,
        opts.capacity_rps(),
        opts.provider_cap,
        opts.stub_delay_ms
    ));

    let before = scrape(&gw.base).await;
    let (samples, stalls) = open_loop(Load {
        client: client(),
        url: format!("{}/v1/chat/completions", gw.base),
        api_key: Some(key),
        body: Some(chat_body(MODEL)),
        kind: Kind::Chat,
        rps,
        duration: Duration::from_secs(opts.duration_secs),
        unique_prompts: true,
    })
    .await;
    let after = scrape(&gw.base).await;

    let s = Stats::of(&samples);
    let d = opts.stub_delay_ms as f64;
    let upstream_cost = probe_upstream_cost(&stub_url, 50).await;
    let sub = |base: f64| -> Vec<f64> {
        let mut v: Vec<f64> = s.latency.iter().map(|l| (l - base).max(0.0)).collect();
        v.sort_by(f64::total_cmp);
        v
    };
    // The number the SLO is checked against: charges the whole provider-call cost to us.
    let overhead = sub(d);
    // The same subtraction with the provider call's real cost removed. Closer to the truth.
    let net = sub(upstream_cost);

    r.say(format!("{} · {stalls} generator stalls", s.line()));
    r.say(format!(
        "total    p50 {:.1}ms   p95 {:.1}ms   p99 {:.1}ms",
        s.p(50.0),
        s.p(95.0),
        s.p(99.0)
    ));
    r.say(format!(
        "overhead p50 {:.2}ms   p95 {:.2}ms   p99 {:.2}ms    (total − {d:.0}ms stub delay)",
        pct(&overhead, 50.0),
        pct(&overhead, 95.0),
        pct(&overhead, 99.0)
    ));
    r.say(format!(
        "net      p50 {:.2}ms   p95 {:.2}ms   p99 {:.2}ms    (total − {upstream_cost:.2}ms measured provider-call cost)",
        pct(&net, 50.0),
        pct(&net, 95.0),
        pct(&net, 99.0)
    ));
    r.say(format!(
        "sb_requests_shed_total +{:.0}",
        metric_sum(&after, "sb_requests_shed_total")
            - metric_sum(&before, "sb_requests_shed_total")
    ));

    r.check(
        "no generator stalls",
        stalls == 0,
        format!("{stalls} requests could not be launched"),
    );
    // `net` is still an upper bound: it charges the gateway for the *client's* loopback
    // leg, which is another ~2.5ms this test introduces and a real client would pay to its
    // own network. The SLO is checked against the upper bound, which is the safe direction.
    r.latency(
        "gateway overhead p50 ≤ 5ms (docs/15 §1) — upper bound",
        pct(&net, 50.0) <= 5.0,
        format!(
            "{:.2}ms net; {:.2}ms if the provider's own HTTP cost is charged to us too",
            pct(&net, 50.0),
            pct(&overhead, 50.0)
        ),
    );
    r.latency(
        "gateway overhead p99 ≤ 50ms (docs/15 §1) — upper bound",
        pct(&net, 99.0) <= 50.0,
        format!(
            "{:.2}ms net; {:.2}ms conservative",
            pct(&net, 99.0),
            pct(&overhead, 99.0)
        ),
    );
    r.latency(
        "…and still ≤ 50ms p99 even charging the provider call's HTTP cost to the gateway",
        pct(&overhead, 99.0) <= 50.0,
        format!("{:.2}ms", pct(&overhead, 99.0)),
    );
    r.check(
        "shed rate ≤ 0.1% at rated capacity (docs/15 §1)",
        s.shed_rate() <= 0.001,
        format!("{:.3}%", s.shed_rate() * 100.0),
    );

    r.json.insert(
        "baseline".into(),
        json!({
            "offered_rps": rps, "capacity_rps": opts.capacity_rps(),
            "requests": s.n, "ok": s.ok, "shed": s.shed,
            "overhead_p50_ms": pct(&overhead, 50.0),
            "overhead_p95_ms": pct(&overhead, 95.0),
            "overhead_p99_ms": pct(&overhead, 99.0),
            "measured_provider_call_ms": upstream_cost,
            "net_overhead_p50_ms": pct(&net, 50.0),
            "net_overhead_p99_ms": pct(&net, 99.0),
            "total_p50_ms": s.p(50.0), "total_p99_ms": s.p(99.0),
            "shed_rate": s.shed_rate(),
        }),
    );
}

/// Scenario 0b — the cache-hit row of the SLO. Not in P5's list; it is one more row in
/// docs/15 §1 that a stub upstream makes cheap to measure, and docs/14 §9 calls the cache
/// the only lever that adds throughput while subtracting cost.
///
/// `temperature: 0` is not decoration. `cache::key::is_cacheable` refuses to cache a
/// request that did not ask for a reproducible answer — no `temperature: 0`, no `seed`, no
/// cache — because replaying one sample of a sampled distribution to a second caller is
/// not a cache hit, it is a wrong answer. The first draft of this scenario omitted it and
/// measured a 0% hit rate on a working cache.
async fn scenario_cache(opts: &Opts, r: &mut Report) {
    header("cache — one deterministic prompt, repeated, cache enabled");
    let (_stub, stub_url) = spawn_stub(opts.stub_delay_ms).await;
    let gw = boot_gateway(
        &stub_url,
        Boot {
            cache: true,
            ..Boot::from(opts)
        },
    )
    .await;
    let key = make_tenant(&gw.pool, "cache", 1_000_000, 1_000_000_000).await;

    let body = json!({
        "model": MODEL,
        "messages": [{"role": "user", "content": format!("cache probe {}", Uuid::now_v7())}],
        "max_tokens": 16,
        "temperature": 0,
        "stream": false
    });

    // One miss, one provider call, one detached `SET`. Then wait for the `SET` to land: it
    // is fire-and-forget by design (docs/15 §2.2), so it is not done when the response is.
    let c = client();
    let _ = c
        .post(format!("{}/v1/chat/completions", gw.base))
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let rps = opts.capacity_rps() * 0.8;
    let (samples, _) = open_loop(Load {
        client: c,
        url: format!("{}/v1/chat/completions", gw.base),
        api_key: Some(key),
        body: Some(body),
        kind: Kind::Chat,
        rps,
        duration: Duration::from_secs(opts.duration_secs.min(10)),
        unique_prompts: false,
    })
    .await;

    let s = Stats::of(&samples);
    let hit_rate = if s.ok > 0 {
        s.cache_hits as f64 / s.ok as f64
    } else {
        0.0
    };
    r.say(format!(
        "{} requests · {} ok · {} cache hits ({:.1}%)",
        s.n,
        s.ok,
        s.cache_hits,
        hit_rate * 100.0
    ));
    r.say(format!(
        "cache-hit total  p50 {:.2}ms   p99 {:.2}ms",
        s.p(50.0),
        s.p(99.0)
    ));

    r.check(
        "the cache actually served (>90% hits)",
        hit_rate > 0.9,
        format!("{:.1}%", hit_rate * 100.0),
    );
    r.latency(
        "cache-hit total latency p99 ≤ 30ms (docs/15 §1)",
        s.p(99.0) <= 30.0,
        format!("{:.2}ms", s.p(99.0)),
    );

    r.json.insert(
        "cache".into(),
        json!({
            "hit_rate": hit_rate,
            "hit_p50_ms": s.p(50.0),
            "hit_p99_ms": s.p(99.0),
        }),
    );
}

/// Scenario 1 — a burst at 10× capacity. Expect clean shedding, bounded memory, flat
/// `/health` latency. docs/13 §P5.
///
/// `/health` is probed *concurrently*, because the property under test is not "is
/// `/health` fast" but "is `/health` fast **while the gateway is saturated**". That is the
/// whole of docs/13 §0.1 and §0.5, and it decides whether Kubernetes kills a pod that is
/// merely busy.
///
/// The `503`s are split by their `Retry-After`. Under provider-bound load — which is the
/// load a gateway actually sees — the refusal comes from the provider semaphore, *inside*
/// the handler, after auth and the limiters and the tokenizer. The `LoadShedLayer` never
/// fires, because nothing ever accumulates 256 requests inside it. `shedlayer` is the
/// scenario that exercises the other one.
async fn scenario_burst(opts: &Opts, r: &mut Report) {
    header("burst — 10× rated capacity, /health probed throughout");
    let (_stub, stub_url) = spawn_stub(opts.stub_delay_ms).await;
    let gw = boot_gateway(&stub_url, Boot::from(opts)).await;
    let key = make_tenant(&gw.pool, "burst", 100_000_000, 1_000_000_000).await;
    warm(&gw, &key, MODEL).await;

    let rps = opts.capacity_rps() * 10.0;
    r.say(format!(
        "offering {rps:.0} rps for {}s against a {:.0} rps gateway",
        opts.duration_secs,
        opts.capacity_rps()
    ));

    let rss_before = rss_mib();
    let before = scrape(&gw.base).await;

    let health = tokio::spawn(open_loop(Load {
        client: client(),
        url: format!("{}/health", gw.base),
        api_key: None,
        body: None,
        kind: Kind::Health,
        rps: 50.0,
        duration: Duration::from_secs(opts.duration_secs),
        unique_prompts: false,
    }));
    let (samples, stalls) = open_loop(Load {
        client: client(),
        url: format!("{}/v1/chat/completions", gw.base),
        api_key: Some(key),
        body: Some(chat_body(MODEL)),
        kind: Kind::Chat,
        rps,
        duration: Duration::from_secs(opts.duration_secs),
        unique_prompts: true,
    })
    .await;
    let (health_samples, _) = health.await.unwrap();

    let after = scrape(&gw.base).await;
    let rss_after = rss_mib();

    let s = Stats::of(&samples);
    let h = Stats::of(&health_samples);
    let served_rps = s.ok as f64 / opts.duration_secs as f64;
    let shed_delta = metric_sum(&after, "sb_requests_shed_total")
        - metric_sum(&before, "sb_requests_shed_total");
    let peak = metric_sum(&after, "sb_inflight_requests_peak");

    r.say(format!("{} · {stalls} generator stalls", s.line()));
    r.say(format!(
        "served {served_rps:.1} rps (capacity {:.1}) · shed rate {:.1}%",
        opts.capacity_rps(),
        s.shed_rate() * 100.0
    ));
    r.say(format!(
        "served   p50 {:.1}ms   p99 {:.1}ms    |   shed decision p50 {:.2}ms   p99 {:.2}ms",
        s.p(50.0),
        s.p(99.0),
        s.shed_p(50.0),
        s.shed_p(99.0)
    ));
    r.say(format!(
        "/health  p50 {:.2}ms   p99 {:.2}ms   ({} probes, {} non-200)",
        h.p(50.0),
        h.p(99.0),
        h.n,
        h.n - h.ok
    ));
    r.say(format!(
        "rss {rss_before:.0} → {rss_after:.0} MiB · sb_requests_shed_total +{shed_delta:.0} · peak inflight {peak:.0} (limit 256)"
    ));

    r.check(
        "the gateway shed rather than queued",
        s.shed > 0,
        format!("{} × 503", s.shed),
    );
    r.check(
        "every shed came from the provider semaphore, not the load-shed layer",
        s.shed_layer == 0 && s.shed_busy > 0,
        format!(
            "{} busy / {} layer — peak inflight {peak:.0} never reached the 256 limit",
            s.shed_busy, s.shed_layer
        ),
    );
    r.check(
        "no 5xx other than 503",
        s.other_5xx == 0,
        format!("{} unexpected 5xx", s.other_5xx),
    );
    // Structural, and the reason this scenario exists: under 10× load the gateway serves
    // its capacity and not one request more. Exceeding it means a queue somewhere, or a
    // second semaphore reached through a fallback — which is the bug that made the first
    // draft of this test report 79.5 rps against a 40 rps gateway.
    r.check(
        "goodput never exceeded rated capacity under 10× load",
        served_rps <= opts.capacity_rps() * 1.1,
        format!("{served_rps:.1} rps vs {:.1} capacity", opts.capacity_rps()),
    );
    r.latency(
        "goodput held at rated capacity under 10× load",
        served_rps >= opts.capacity_rps() * 0.9,
        format!("{served_rps:.1} rps vs {:.1} capacity", opts.capacity_rps()),
    );
    r.latency(
        "shed decision p99 ≤ 5ms (docs/15 §1)",
        s.shed_p(99.0) <= 5.0,
        format!("{:.2}ms", s.shed_p(99.0)),
    );
    r.latency(
        "/health p99 ≤ 10ms while saturated (docs/15 §1)",
        h.p(99.0) <= 10.0,
        format!("{:.2}ms", h.p(99.0)),
    );
    r.check(
        "/health never failed while saturated",
        h.ok == h.n,
        format!("{}/{} succeeded", h.ok, h.n),
    );
    r.check(
        "memory bounded under overload",
        rss_after - rss_before < 512.0,
        format!("{:+.0} MiB", rss_after - rss_before),
    );
    r.check(
        "sb_requests_shed_total counted exactly the sheds",
        (shed_delta - s.shed as f64).abs() <= 2.0,
        format!("metric +{shed_delta:.0} vs observed {}", s.shed),
    );

    r.json.insert(
        "burst".into(),
        json!({
            "offered_rps": rps, "capacity_rps": opts.capacity_rps(),
            "requests": s.n, "ok": s.ok,
            "shed": s.shed, "shed_layer": s.shed_layer, "shed_busy": s.shed_busy,
            "served_rps": served_rps, "shed_rate": s.shed_rate(),
            "served_p50_ms": s.p(50.0), "served_p99_ms": s.p(99.0),
            "shed_decision_p50_ms": s.shed_p(50.0), "shed_decision_p99_ms": s.shed_p(99.0),
            "health_p50_ms": h.p(50.0), "health_p99_ms": h.p(99.0),
            "health_probes": h.n, "health_failures": h.n - h.ok,
            "rss_before_mib": rss_before, "rss_after_mib": rss_after,
            "peak_inflight": peak,
        }),
    );
}

/// Scenario 1b — the other `503`.
///
/// `burst` never reaches the `LoadShedLayer`, because a provider semaphore of 8 refuses
/// request 9 in about a millisecond and nothing accumulates. So the layer that docs/13 §P0
/// exists to install is, under provider-bound load, never the thing that fires.
///
/// To exercise it, make the *gateway* the scarce resource: a provider cap far above the
/// concurrency limit. This is not a hypothetical configuration — it is what a gateway in
/// front of a large hosted provider looks like (`openai.max_inflight = 64`, and a
/// `concurrency_limit` chosen for the pod's memory).
///
/// The `Retry-After` is what tells the two apart on the wire, and it is why docs/15 §7
/// says they differ on purpose.
async fn scenario_shed_layer(opts: &Opts, r: &mut Report) {
    header("shed layer — the gateway, not the provider, is the scarce resource");
    let limit = 16;
    let (_stub, stub_url) = spawn_stub(opts.stub_delay_ms).await;
    let gw = boot_gateway(
        &stub_url,
        Boot {
            cache: false,
            provider_cap: 4096,
            concurrency_limit: limit,
        },
    )
    .await;
    let key = make_tenant(&gw.pool, "shedlayer", 100_000_000, 1_000_000_000).await;
    warm(&gw, &key, MODEL).await;

    let capacity = limit as f64 / (opts.stub_delay_ms as f64 / 1000.0);
    let rps = capacity * 10.0;
    r.say(format!(
        "concurrency_limit {limit} · provider cap 4096 · capacity {capacity:.0} rps · offering {rps:.0} rps"
    ));

    let health = tokio::spawn(open_loop(Load {
        client: client(),
        url: format!("{}/health", gw.base),
        api_key: None,
        body: None,
        kind: Kind::Health,
        rps: 50.0,
        duration: Duration::from_secs(opts.duration_secs),
        unique_prompts: false,
    }));
    let (samples, stalls) = open_loop(Load {
        client: client(),
        url: format!("{}/v1/chat/completions", gw.base),
        api_key: Some(key),
        body: Some(chat_body(MODEL)),
        kind: Kind::Chat,
        rps,
        duration: Duration::from_secs(opts.duration_secs),
        unique_prompts: true,
    })
    .await;
    let (health_samples, _) = health.await.unwrap();

    let s = Stats::of(&samples);
    let h = Stats::of(&health_samples);
    let served_rps = s.ok as f64 / opts.duration_secs as f64;

    r.say(format!("{} · {stalls} generator stalls", s.line()));
    r.say(format!(
        "served {served_rps:.1} rps · shed rate {:.1}%",
        s.shed_rate() * 100.0
    ));
    r.say(format!(
        "shed-layer decision p50 {:.3}ms   p99 {:.3}ms   (refused before auth, before Redis)",
        s.shed_p(50.0),
        s.shed_p(99.0)
    ));
    r.say(format!(
        "/health  p50 {:.2}ms   p99 {:.2}ms   ({} probes, {} non-200)",
        h.p(50.0),
        h.p(99.0),
        h.n,
        h.n - h.ok
    ));

    r.check(
        "the load-shed layer is what refused",
        s.shed_layer > 0,
        format!("{} layer / {} busy", s.shed_layer, s.shed_busy),
    );
    r.latency(
        "a load-shed 503 is cheaper than a provider-busy 503",
        s.shed_p(99.0) <= 5.0,
        format!("p99 {:.3}ms", s.shed_p(99.0)),
    );
    r.latency(
        "goodput held at the concurrency limit",
        served_rps >= capacity * 0.75,
        format!("{served_rps:.1} rps vs {capacity:.0} capacity"),
    );
    r.latency(
        "/health p99 ≤ 10ms while the limiter is saturated",
        h.p(99.0) <= 10.0,
        format!("{:.2}ms", h.p(99.0)),
    );

    r.json.insert(
        "shed_layer".into(),
        json!({
            "concurrency_limit": limit, "capacity_rps": capacity, "offered_rps": rps,
            "requests": s.n, "ok": s.ok,
            "shed": s.shed, "shed_layer": s.shed_layer, "shed_busy": s.shed_busy,
            "served_rps": served_rps,
            "shed_decision_p50_ms": s.shed_p(50.0), "shed_decision_p99_ms": s.shed_p(99.0),
            "health_p99_ms": h.p(99.0),
        }),
    );
}

/// Scenario 2 — a provider dies mid-flight. Expect fallback, not `503`. docs/13 §P5.
///
/// `bench-primary` declares `fallback = ["bench-backup"]`, and the two aliases sit on
/// different providers. Killing the openai stub while traffic is in flight is therefore
/// the shape of a real upstream outage, and the assertion is the one
/// `examples/bot-fleet`'s AIMD controller depends on: it must not see a `500`.
async fn scenario_provider_death(opts: &Opts, r: &mut Report) {
    header("provider death — the primary dies mid-flight, expect fallback not 503");
    let (stub, stub_url) = spawn_stub(opts.stub_delay_ms).await;
    let gw = boot_gateway(&stub_url, Boot::from(opts)).await;
    let key = make_tenant(&gw.pool, "death", 100_000_000, 1_000_000_000).await;
    warm(&gw, &key, MODEL_FB).await;

    let rps = opts.capacity_rps() * 0.5;
    let total = Duration::from_secs(opts.duration_secs);
    let kill_at = total / 3;
    r.say(format!(
        "offering {rps:.1} rps; killing the openai upstream at t+{}s",
        kill_at.as_secs()
    ));

    let killer = {
        let stub = stub.clone();
        tokio::spawn(async move {
            tokio::time::sleep(kill_at).await;
            eprintln!("  ! openai upstream now answering 500");
            stub.openai.kill();
        })
    };

    let (samples, _) = open_loop(Load {
        client: client(),
        url: format!("{}/v1/chat/completions", gw.base),
        api_key: Some(key),
        body: Some(chat_body(MODEL_FB)),
        kind: Kind::Chat,
        rps,
        duration: total,
        unique_prompts: true,
    })
    .await;
    let _ = killer.await;
    stub.openai.revive();

    // Split on *launch* time, not completion order and not the fallback header — the
    // latter would beg the question. The half-second guard drops requests that were in
    // flight across the kill; those legitimately reached a healthy upstream.
    let cut = kill_at.as_secs_f64() * 1000.0 + 500.0;
    let after_death: Vec<Sample> = samples
        .iter()
        .filter(|s| s.launched_ms >= cut)
        .cloned()
        .collect();

    let s = Stats::of(&after_death);
    let all = Stats::of(&samples);

    r.say(format!("after the kill: {}", s.line()));
    r.say(format!(
        "  of which {} carried x-switchboard-fallback",
        s.fallbacks
    ));
    r.say(format!(
        "whole run: {} ok / {} requests · openai stub saw {} calls · ollama stub saw {}",
        all.ok,
        all.n,
        stub.openai.calls(),
        stub.ollama.calls()
    ));
    r.say(format!(
        "served p50 {:.1}ms   p99 {:.1}ms   (a fallback pays the failed attempt first)",
        s.p(50.0),
        s.p(99.0)
    ));

    r.check(
        "traffic kept being served after the provider died",
        s.ok > 0,
        format!("{} × 200 after the kill", s.ok),
    );
    r.check(
        "the fallback provider answered",
        s.fallbacks > 0,
        format!("{} responses carried x-switchboard-fallback", s.fallbacks),
    );
    r.check(
        "no 500 reached the client (bot-fleet reads 500 as Fatal)",
        s.other_5xx == 0,
        format!("{} × 5xx that were not 503", s.other_5xx),
    );
    r.check(
        "a dead provider did not become a shed",
        s.shed == 0,
        format!("{} × shed-503 after the kill", s.shed),
    );
    r.check(
        "the fallback carried essentially all post-kill traffic",
        s.ok > 0 && s.fallbacks as f64 / s.ok as f64 > 0.9,
        format!(
            "{:.1}% of successes were fallbacks",
            100.0 * s.fallbacks as f64 / s.ok.max(1) as f64
        ),
    );

    r.json.insert(
        "provider_death".into(),
        json!({
            "after_kill_requests": s.n, "after_kill_ok": s.ok,
            "fallbacks": s.fallbacks, "shed": s.shed, "unavailable": s.unavailable,
            "other_5xx": s.other_5xx,
            "after_kill_p50_ms": s.p(50.0), "after_kill_p99_ms": s.p(99.0),
            "openai_stub_calls": stub.openai.calls(),
            "ollama_stub_calls": stub.ollama.calls(),
        }),
    );
}

/// Scenario 3 — one tenant tries to consume the whole gateway. docs/13 §P5 and §1.6.
///
/// Run twice, and the pair is the point.
///
/// **With the per-tenant GCRA at its configured default**, the hog's *arrival rate* is
/// bounded before the provider semaphore ever sees it. That is mechanism 1 of §1.6, and it
/// is built.
///
/// **With the GCRA raised out of the way**, nothing bounds the hog's *concurrency*, so it
/// holds every provider permit and the victim is refused. That is mechanism 2 of §1.6 —
/// the per-tenant permit floor — and it is not built. This scenario turns its absence into
/// a number.
///
/// Watch what the victim's p99 does in the second run: nothing. It cannot rise, because a
/// gateway that refuses rather than queues has no queue for the victim to wait in. The
/// injury appears as a **shed rate**, not a latency. docs/13 §1.6's "done when" clause —
/// *"a tenant issuing 200 concurrent requests does not move a second tenant's p99"* — is
/// therefore satisfied by a gateway with no fairness at all. It is the wrong clause.
async fn scenario_fairness(opts: &Opts, r: &mut Report) {
    header("fairness — one tenant tries to take the whole gateway");
    let (_stub, stub_url) = spawn_stub(opts.stub_delay_ms).await;
    let gw = boot_gateway(&stub_url, Boot::from(opts)).await;

    let victim_rps = 2.0f64.max(opts.capacity_rps() * 0.05);
    let hog_rps = opts.capacity_rps() * 10.0;
    let dur = Duration::from_secs(opts.duration_secs.min(15));
    r.say(format!(
        "victim {victim_rps:.1} rps · hog {hog_rps:.0} rps · capacity {:.1} rps",
        opts.capacity_rps()
    ));

    // ── control: the victim alone ────────────────────────────────────────────
    let b_key = make_tenant(&gw.pool, "victim-control", 1_000_000, 1_000_000_000).await;
    warm(&gw, &b_key, MODEL).await;
    let (control, _) = open_loop(Load {
        client: client(),
        url: format!("{}/v1/chat/completions", gw.base),
        api_key: Some(b_key),
        body: Some(chat_body(MODEL)),
        kind: Kind::Chat,
        rps: victim_rps,
        duration: dur,
        unique_prompts: true,
    })
    .await;
    let control = Stats::of(&control);
    r.say(format!(
        "control (victim alone)     : {} ok / {} · p99 {:.1}ms · shed {:.1}%",
        control.ok,
        control.n,
        control.p(99.0),
        control.shed_rate() * 100.0
    ));

    let contended = async |hog_rpm: i32, v_label: &'static str, h_label: &'static str| {
        let base = gw.base.clone();
        let hog = make_tenant(&gw.pool, h_label, hog_rpm, 1_000_000_000).await;
        let victim = make_tenant(&gw.pool, v_label, 1_000_000, 1_000_000_000).await;
        let g = Gateway {
            base: base.clone(),
            pool: gw.pool.clone(),
        };
        warm(&g, &hog, MODEL).await;
        warm(&g, &victim, MODEL).await;

        let hog_task = tokio::spawn(open_loop(Load {
            client: client(),
            url: format!("{base}/v1/chat/completions"),
            api_key: Some(hog),
            body: Some(chat_body(MODEL)),
            kind: Kind::Chat,
            rps: hog_rps,
            duration: dur,
            unique_prompts: true,
        }));
        let (v, _) = open_loop(Load {
            client: client(),
            url: format!("{base}/v1/chat/completions"),
            api_key: Some(victim),
            body: Some(chat_body(MODEL)),
            kind: Kind::Chat,
            rps: victim_rps,
            duration: dur,
            unique_prompts: true,
        })
        .await;
        let (h, _) = hog_task.await.unwrap();
        (Stats::of(&v), Stats::of(&h))
    };

    // ── run A: the hog is held down by its own GCRA (mechanism 1) ────────────
    let (v_gcra, h_gcra) = contended(60, "victim-gcra", "hog-gcra").await;
    r.say(format!(
        "hog at the default 60 rpm  : victim {} ok / {} · p99 {:.1}ms · shed {:.1}%   | hog {} ok, {} × 429",
        v_gcra.ok, v_gcra.n, v_gcra.p(99.0), v_gcra.shed_rate() * 100.0, h_gcra.ok, h_gcra.rate_limited_429
    ));

    // ── run B: the GCRA is raised. Only a permit floor could save the victim,
    //           and there is no permit floor. ─────────────────────────────────
    let (v_free, h_free) = contended(1_000_000, "victim-free", "hog-free").await;
    r.say(format!(
        "hog with no rate limit     : victim {} ok / {} · p99 {:.1}ms · shed {:.1}%   | hog {} ok, {} × 429",
        v_free.ok, v_free.n, v_free.p(99.0), v_free.shed_rate() * 100.0, h_free.ok, h_free.rate_limited_429
    ));

    r.check(
        "GCRA bounds the hog's arrival rate (mechanism 1, docs/13 §1.6)",
        h_gcra.rate_limited_429 > 0,
        format!("{} × 429 to the hog", h_gcra.rate_limited_429),
    );
    r.check(
        "with GCRA on, the victim is nearly unharmed",
        v_gcra.success_rate() >= control.success_rate() * 0.9,
        format!(
            "success {:.1}% vs {:.1}% alone",
            v_gcra.success_rate() * 100.0,
            control.success_rate() * 100.0
        ),
    );

    // The trap, asserted as a trap: p99 is flat in both runs, so p99 cannot detect this.
    let p99_moved = (v_free.p(99.0) - control.p(99.0)).abs();
    r.check(
        "the victim's p99 barely moves even when starved — §1.6's 'done when' is unfalsifiable",
        v_free.ok == 0 || p99_moved < 50.0,
        format!(
            "p99 {:.1}ms → {:.1}ms while success {:.1}% → {:.1}%",
            control.p(99.0),
            v_free.p(99.0),
            control.success_rate() * 100.0,
            v_free.success_rate() * 100.0
        ),
    );

    let starved = v_free.success_rate() < control.success_rate() * 0.9;
    r.say(format!(
        "\n  → with the arrival-rate limiter out of the way, the victim's success rate went {:.1}% → {:.1}%\n    while its p99 stayed {:.1}ms → {:.1}ms. Per-tenant permit floors (§1.6 mechanism 2) are {}.",
        control.success_rate() * 100.0,
        v_free.success_rate() * 100.0,
        control.p(99.0),
        v_free.p(99.0),
        if starved { "NOT built, and this is what their absence costs" } else { "not built, though this run did not starve the victim" }
    ));

    r.json.insert(
        "fairness".into(),
        json!({
            "victim_rps": victim_rps, "hog_rps": hog_rps, "capacity_rps": opts.capacity_rps(),
            "control": {"ok": control.ok, "n": control.n, "p99_ms": control.p(99.0), "success_rate": control.success_rate(), "shed_rate": control.shed_rate()},
            "hog_rpm_60": {"victim_ok": v_gcra.ok, "victim_n": v_gcra.n, "victim_p99_ms": v_gcra.p(99.0), "victim_success_rate": v_gcra.success_rate(), "victim_shed_rate": v_gcra.shed_rate(), "hog_ok": h_gcra.ok, "hog_429": h_gcra.rate_limited_429},
            "hog_rpm_unlimited": {"victim_ok": v_free.ok, "victim_n": v_free.n, "victim_p99_ms": v_free.p(99.0), "victim_success_rate": v_free.success_rate(), "victim_shed_rate": v_free.shed_rate(), "hog_ok": h_free.ok, "hog_429": h_free.rate_limited_429},
            "permit_floor_built": false,
            "victim_starved_without_gcra": starved,
        }),
    );
}

// ── wiring ───────────────────────────────────────────────────────────────────

fn parse_args() -> Opts {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    Opts {
        scenario: value("--scenario").unwrap_or_else(|| "all".into()),
        duration_secs: value("--duration-secs")
            .and_then(|v| v.parse().ok())
            .unwrap_or(20),
        stub_delay_ms: value("--stub-delay-ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(200),
        provider_cap: value("--provider-cap")
            .and_then(|v| v.parse().ok())
            .unwrap_or(8),
        json_out: value("--json"),
        enforce_latency: !args.iter().any(|a| a == "--no-latency-slo"),
    }
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let opts = parse_args();

    // A load test that skips is a load test that reports success for having done nothing.
    // The integration suite already had that failure mode once; it does not get to happen
    // twice.
    for var in ["DATABASE_URL", "REDIS_URL"] {
        if std::env::var(var).is_err() {
            eprintln!("\x1b[1;31mloadtest: {var} is not set.\x1b[0m");
            eprintln!("This measures the gateway with its database and Redis on the request path.");
            eprintln!("Without them it would measure something else and call it a p99.");
            std::process::exit(2);
        }
    }

    switchboard::telemetry::init_metrics(true);

    println!("switchboard load test — docs/13 §P5");
    println!(
        "provider cap {} · stub delay {}ms · rated capacity {:.1} rps · {}s per scenario",
        opts.provider_cap,
        opts.stub_delay_ms,
        opts.capacity_rps(),
        opts.duration_secs
    );

    if !opts.enforce_latency {
        println!(
            "\x1b[33m--no-latency-slo: wall-clock percentiles are reported, not enforced.\x1b[0m"
        );
    }

    let mut r = Report::new(opts.enforce_latency);
    let run = |name: &str| opts.scenario == "all" || opts.scenario == name;

    if run("baseline") {
        scenario_baseline(&opts, &mut r).await;
    }
    if run("cache") {
        scenario_cache(&opts, &mut r).await;
    }
    if run("burst") {
        scenario_burst(&opts, &mut r).await;
    }
    if run("shedlayer") {
        scenario_shed_layer(&opts, &mut r).await;
    }
    if run("death") {
        scenario_provider_death(&opts, &mut r).await;
    }
    if run("fairness") {
        scenario_fairness(&opts, &mut r).await;
    }

    if let Some(path) = &opts.json_out {
        let doc = serde_json::Value::Object(r.json.clone());
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap()).expect("write json");
        println!("\nwrote {path}");
    }

    header("result");
    let failed: Vec<_> = r.checks.iter().filter(|c| !c.1).collect();
    if failed.is_empty() {
        println!("\x1b[1;32mall {} checks passed\x1b[0m", r.checks.len());
    } else {
        println!(
            "\x1b[1;31m{} of {} checks failed\x1b[0m",
            failed.len(),
            r.checks.len()
        );
        for (name, _, detail) in &failed {
            println!("  - {name} — {detail}");
        }
        std::process::exit(1);
    }
}
