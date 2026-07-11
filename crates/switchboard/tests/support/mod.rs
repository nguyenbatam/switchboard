//! A real gateway, over a real socket, against a real upstream.
//!
//! Everything here talks HTTP. Calling the handler functions directly would skip
//! the middleware, the SSE body, and the connection lifecycle — and the connection
//! lifecycle is precisely what decides whether a streamed request gets billed.
//!
//! Requires `DATABASE_URL`; the usage assertions have nothing to read without it.

use axum::Router;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use sqlx::PgPool;
use std::sync::Once;
use std::time::Duration;
use switchboard::AppConfig;
use switchboard::app::AppState;
use uuid::Uuid;

/// The upstream sends 10 deltas, 150ms apart, so a test can hang up in the middle
/// of an answer the way a real client does.
pub const UPSTREAM_DELTAS: [&str; 10] = [
    "Hello",
    " there",
    ",",
    " this",
    " is",
    " a",
    " streamed",
    " answer",
    " for",
    " you",
];
pub const UPSTREAM_TEXT: &str = "Hello there, this is a streamed answer for you";
pub const UPSTREAM_PROMPT_TOKENS: u32 = 11;
pub const UPSTREAM_COMPLETION_TOKENS: u32 = 10;
const DELTA_GAP: Duration = Duration::from_millis(150);

fn ensure_api_key_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // The openai provider only registers when a key is present. Process-global,
        // hence the `Once`: every test in this binary wants the same value.
        unsafe { std::env::set_var("OPENAI_API_KEY", "test-key") };
    });
}

// ── The fake upstream ────────────────────────────────────────────────────────

async fn upstream_chat(body: axum::Json<serde_json::Value>) -> Response {
    let model = body.0["model"].as_str().unwrap_or_default().to_string();
    let stream = body.0["stream"].as_bool().unwrap_or(false);

    if model == "flaky-model" {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
    }
    if model == "overloaded-model" {
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "busy").into_response();
    }

    if !stream {
        return axum::Json(serde_json::json!({
            "id": "chatcmpl-upstream",
            "object": "chat.completion",
            "created": 1700000000,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": UPSTREAM_TEXT},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": UPSTREAM_PROMPT_TOKENS,
                "completion_tokens": UPSTREAM_COMPLETION_TOKENS,
                "total_tokens": UPSTREAM_PROMPT_TOKENS + UPSTREAM_COMPLETION_TOKENS
            }
        }))
        .into_response();
    }

    let sse = async_stream::stream! {
        for d in UPSTREAM_DELTAS {
            let chunk = serde_json::json!({
                "id": "chatcmpl-upstream",
                "model": "upstream",
                "created": 1700000000,
                "choices": [{"index": 0, "delta": {"content": d}, "finish_reason": null}]
            });
            yield Ok::<_, std::convert::Infallible>(
                axum::body::Bytes::from(format!("data: {chunk}\n\n"))
            );
            tokio::time::sleep(DELTA_GAP).await;
        }
        let finish = serde_json::json!({
            "id": "chatcmpl-upstream",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        });
        yield Ok(axum::body::Bytes::from(format!("data: {finish}\n\n")));

        let usage = serde_json::json!({
            "id": "chatcmpl-upstream",
            "choices": [],
            "usage": {
                "prompt_tokens": UPSTREAM_PROMPT_TOKENS,
                "completion_tokens": UPSTREAM_COMPLETION_TOKENS,
                "total_tokens": UPSTREAM_PROMPT_TOKENS + UPSTREAM_COMPLETION_TOKENS
            }
        });
        yield Ok(axum::body::Bytes::from(format!("data: {usage}\n\n")));
        yield Ok(axum::body::Bytes::from_static(b"data: [DONE]\n\n"));
    };

    Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from_stream(sse))
        .expect("sse response")
}

async fn upstream_embeddings(body: axum::Json<serde_json::Value>) -> Response {
    let n = body.0["input"].as_array().map(|a| a.len()).unwrap_or(1);
    let data: Vec<_> = (0..n)
        .map(|i| serde_json::json!({"index": i, "object": "embedding", "embedding": [0.5, -0.25]}))
        .collect();
    axum::Json(serde_json::json!({
        "object": "list",
        "model": body.0["model"],
        "data": data,
        "usage": {"prompt_tokens": 3, "completion_tokens": 0, "total_tokens": 3}
    }))
    .into_response()
}

async fn spawn_upstream() -> String {
    let app = Router::new()
        .route("/v1/chat/completions", post(upstream_chat))
        .route("/v1/embeddings", post(upstream_embeddings));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/v1")
}

// ── The gateway under test ───────────────────────────────────────────────────

const TEST_MODELS: &str = r#"
[[model]]
alias = "fast"
provider = "openai"
upstream_name = "fast-model"
context = 8192
price_in = 0.15
price_out = 0.60
tokenizer = { kind = "tiktoken", encoding = "o200k_base" }

[[model]]
alias = "flaky"
provider = "openai"
upstream_name = "flaky-model"
context = 8192
price_in = 1.0
price_out = 2.0
tokenizer = { kind = "tiktoken", encoding = "o200k_base" }
fallback = ["backup"]

[[model]]
alias = "backup"
provider = "openai"
upstream_name = "backup-model"
context = 8192
price_in = 0.5
price_out = 1.0
tokenizer = { kind = "tiktoken", encoding = "o200k_base" }

[[model]]
alias = "doomed"
provider = "openai"
upstream_name = "flaky-model"
context = 8192
price_in = 1.0
price_out = 2.0
tokenizer = { kind = "tiktoken", encoding = "o200k_base" }

[[model]]
alias = "embedder"
provider = "openai"
upstream_name = "embed-model"
context = 8192
price_in = 0.0
price_out = 0.0
tokenizer = { kind = "estimate" }
"#;

pub struct Gateway {
    pub base: String,
    pub client: reqwest::Client,
    pub pool: PgPool,
    /// Salts every prompt, because the exact cache is backed by a Redis that
    /// outlives the test process. Without it, a "miss" in one run is served from
    /// the previous run's entry and the cache assertions verify nothing.
    nonce: Uuid,
}

/// Boots a gateway on an ephemeral port. `None` when there is no database to
/// assert against — the caller should skip rather than pretend it verified something.
pub async fn spawn(cache_enabled: bool) -> Option<Gateway> {
    // Redis is not optional here: idempotency and the shared cache both live in it,
    // and a gateway booted without it would quietly skip the behaviour under test.
    if std::env::var("DATABASE_URL").is_err() || std::env::var("REDIS_URL").is_err() {
        // A skip that CI cannot see is a test that cannot fail. Locally, skipping
        // is a convenience; in CI it means the suite silently verified nothing.
        assert!(
            std::env::var("CI").is_err(),
            "DATABASE_URL/REDIS_URL unset in CI: these tests would skip, not pass"
        );
        eprintln!("DATABASE_URL or REDIS_URL unset, skipping HTTP integration test");
        return None;
    }
    ensure_api_key_env();

    let upstream = spawn_upstream().await;

    let models_path = std::env::temp_dir().join(format!("sb-models-{}.toml", Uuid::now_v7()));
    std::fs::write(&models_path, TEST_MODELS).expect("write test models");

    let mut config = AppConfig::load().expect("load config");
    config.models_path = models_path.to_string_lossy().into_owned();
    config.providers.openai.base_url = upstream;
    config.cache.enabled = cache_enabled;
    config.agent.enabled = false;
    config.auth.require_auth = false;

    let mut state = AppState::boot(config).await.expect("boot gateway");
    let pool = state.db.clone().expect("database");

    // Every test here shares the dev tenant, so a shared token bucket would couple
    // them: enough tests in one minute and an unrelated one starts seeing 429s.
    // Rate limiting has its own suite (`tests/ratelimit_gcra.rs`).
    state.rate_limiter = None;

    let app = switchboard::http::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Some(Gateway {
        base: format!("http://{addr}"),
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap(),
        pool,
        nonce: Uuid::now_v7(),
    })
}

macro_rules! gateway_or_skip {
    ($cache:expr) => {
        match crate::support::spawn($cache).await {
            Some(g) => g,
            None => return,
        }
    };
}
pub(crate) use gateway_or_skip;

impl Gateway {
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// A fresh id per call, so concurrent tests never read each other's usage rows.
    pub fn request_id(&self, label: &str) -> String {
        format!("it-{label}-{}", Uuid::now_v7())
    }

    /// A prompt unique to this gateway instance. Two calls with the same `text`
    /// produce the same prompt (so a cache hit is possible), but no other test —
    /// and no earlier run — can have cached it.
    pub fn prompt(&self, text: &str) -> String {
        format!("{text} [{}]", self.nonce)
    }

    pub fn chat(&self, request_id: &str, body: serde_json::Value) -> reqwest::RequestBuilder {
        self.client
            .post(self.url("/v1/chat/completions"))
            .header("x-request-id", request_id)
            .json(&body)
    }

    /// Usage is written from a spawned task after the response is on the wire, so
    /// the row lands a little after the request returns.
    pub async fn await_usage(&self, request_id: &str) -> UsageRow {
        for _ in 0..100 {
            if let Ok(Some(row)) = sqlx::query_as::<_, UsageRow>(
                "SELECT status, prompt_tokens, completion_tokens, cost_nano, cache_hit, \
                 served_by_alias, model_alias, finish_reason \
                 FROM usage_records WHERE request_id = $1",
            )
            .bind(request_id)
            .fetch_optional(&self.pool)
            .await
            {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no usage row was ever written for {request_id}");
    }

    pub async fn usage_row_count(&self, request_id: &str) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM usage_records WHERE request_id = $1")
            .bind(request_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// Creates a tenant with a monthly ceiling and an API key that authenticates as it.
    pub async fn tenant_with_budget(&self, budget_nano: i64, spent_nano: i64) -> String {
        let tenant = Uuid::now_v7();
        sqlx::query("INSERT INTO tenants (id, name, monthly_budget_nano) VALUES ($1, $2, $3)")
            .bind(tenant)
            .bind(format!("it-{tenant}"))
            .bind(budget_nano)
            .execute(&self.pool)
            .await
            .unwrap();

        if spent_nano > 0 {
            sqlx::query(
                "INSERT INTO usage_hourly (tenant_id, hour, model_alias, requests, cost_nano) \
                 VALUES ($1, date_trunc('hour', now()), 'fast', 1, $2)",
            )
            .bind(tenant)
            .bind(spent_nano)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        let (prefix, full_key) = switchboard::auth::key::generate_key();
        let secret = full_key.split_once('.').map(|(_, s)| s).unwrap();
        let hash = switchboard::auth::key::hash_secret(secret).unwrap();
        sqlx::query(
            "INSERT INTO api_keys (id, tenant_id, key_prefix, key_hash, name) \
             VALUES ($1, $2, $3, $4, 'it')",
        )
        .bind(Uuid::now_v7())
        .bind(tenant)
        .bind(prefix)
        .bind(hash)
        .execute(&self.pool)
        .await
        .unwrap();

        full_key
    }
}

#[derive(Debug, sqlx::FromRow)]
pub struct UsageRow {
    pub status: String,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub cost_nano: i64,
    pub cache_hit: Option<String>,
    pub served_by_alias: String,
    pub model_alias: String,
    pub finish_reason: Option<String>,
}
