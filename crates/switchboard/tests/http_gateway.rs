//! End-to-end HTTP tests: real socket, real SSE, real usage rows.
//!
//! These cover the failures that only appear once a connection exists — a client
//! that hangs up mid-stream, a cached response replayed as SSE, a fallback that
//! must not be billed at the requested model's price. None of them are reachable
//! by calling the handler function directly.

mod support;

use futures::StreamExt;
use serde_json::json;
use support::{
    Gateway, UPSTREAM_COMPLETION_TOKENS, UPSTREAM_PROMPT_TOKENS, UPSTREAM_TEXT, gateway_or_skip,
};

fn user(content: &str) -> serde_json::Value {
    json!([{"role": "user", "content": content}])
}

async fn read_sse(resp: reqwest::Response) -> String {
    String::from_utf8_lossy(&resp.bytes().await.unwrap()).into_owned()
}

// ── Streaming is billed ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_completed_stream_is_billed() {
    let gw: Gateway = gateway_or_skip!(false);
    let rid = gw.request_id("stream-billed");

    let resp = gw
        .chat(
            &rid,
            json!({"model": "fast", "messages": user("hi"), "stream": true}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body = read_sse(resp).await;
    assert!(body.ends_with("data: [DONE]\n\n"), "SSE must terminate");
    assert!(body.contains("Hello"));

    let row = gw.await_usage(&rid).await;
    assert_eq!(row.status, "completed");
    assert_eq!(row.prompt_tokens, UPSTREAM_PROMPT_TOKENS as i32);
    assert_eq!(row.completion_tokens, UPSTREAM_COMPLETION_TOKENS as i32);
    // 11 * $0.15/M + 10 * $0.60/M, in nanodollars.
    assert_eq!(row.cost_nano, 7650);
    assert_eq!(row.finish_reason.as_deref(), Some("stop"));
}

#[tokio::test]
async fn a_client_that_hangs_up_mid_stream_is_still_billed_for_what_it_received() {
    let gw: Gateway = gateway_or_skip!(false);
    let rid = gw.request_id("disconnect");

    let resp = gw
        .chat(
            &rid,
            json!({"model": "fast", "messages": user("hi"), "stream": true}),
        )
        .send()
        .await
        .unwrap();

    // Take a couple of deltas, then vanish — the upstream is still generating.
    let mut stream = resp.bytes_stream();
    let mut seen = 0;
    while seen < 2 {
        match stream.next().await {
            Some(Ok(_)) => seen += 1,
            _ => break,
        }
    }
    drop(stream);

    let row = gw.await_usage(&rid).await;
    assert_eq!(
        row.status, "client_disconnect",
        "the upstream generated tokens; somebody has to pay for them"
    );
    assert!(
        row.completion_tokens > 0,
        "tokens crossed the wire before the client left"
    );
    assert!(
        row.completion_tokens < UPSTREAM_COMPLETION_TOKENS as i32,
        "but not all of them: expected a partial answer, got {}",
        row.completion_tokens
    );
    assert!(row.cost_nano > 0);
}

#[tokio::test]
async fn exactly_one_usage_row_is_written_per_streamed_request() {
    let gw: Gateway = gateway_or_skip!(false);
    let rid = gw.request_id("one-row");

    let resp = gw
        .chat(
            &rid,
            json!({"model": "fast", "messages": user("hi"), "stream": true}),
        )
        .send()
        .await
        .unwrap();
    let _ = read_sse(resp).await;

    gw.await_usage(&rid).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(gw.usage_row_count(&rid).await, 1, "no double billing");
}

// ── Cache ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_cache_hit_is_free() {
    let gw: Gateway = gateway_or_skip!(true);
    let miss = gw.request_id("cache-miss");
    let hit = gw.request_id("cache-hit");
    let body = json!({"model": "fast", "messages": user(&gw.prompt("cache me")), "temperature": 0});

    let r1 = gw.chat(&miss, body.clone()).send().await.unwrap();
    assert_eq!(r1.status(), 200);
    let first = gw.await_usage(&miss).await;
    assert!(first.cache_hit.is_none());
    assert_eq!(first.cost_nano, 7650);

    let r2 = gw.chat(&hit, body).send().await.unwrap();
    assert_eq!(r2.status(), 200);
    let second = gw.await_usage(&hit).await;

    assert_eq!(second.cache_hit.as_deref(), Some("exact"));
    assert_eq!(second.cost_nano, 0, "a hit consumed no upstream tokens");
    assert_eq!(second.prompt_tokens, 0);
    assert_eq!(second.completion_tokens, 0);
}

#[tokio::test]
async fn a_streamed_response_seeds_the_cache_and_the_next_stream_hits_it() {
    let gw: Gateway = gateway_or_skip!(true);
    let miss = gw.request_id("stream-seed");
    let hit = gw.request_id("stream-hit");
    let body = json!({
        "model": "fast", "messages": user(&gw.prompt("seed from a stream")),
        "temperature": 0, "stream": true
    });

    let started = std::time::Instant::now();
    let r1 = gw.chat(&miss, body.clone()).send().await.unwrap();
    let _ = read_sse(r1).await;
    let cold = started.elapsed();
    gw.await_usage(&miss).await;

    let started = std::time::Instant::now();
    let r2 = gw.chat(&hit, body).send().await.unwrap();
    let warm_body = read_sse(r2).await;
    let warm = started.elapsed();

    let row = gw.await_usage(&hit).await;
    assert_eq!(
        row.cache_hit.as_deref(),
        Some("exact"),
        "production streams; a cache that only fills from non-streaming requests never fills"
    );
    assert!(warm_body.contains(UPSTREAM_TEXT), "the hit replays as SSE");
    assert!(warm_body.ends_with("data: [DONE]\n\n"));
    assert!(
        warm < cold / 2,
        "a hit must not wait on the upstream: cold={cold:?} warm={warm:?}"
    );
}

#[tokio::test]
async fn a_sampled_request_is_not_cached() {
    let gw: Gateway = gateway_or_skip!(true);
    let first = gw.request_id("sampled-1");
    let second = gw.request_id("sampled-2");
    let body =
        json!({"model": "fast", "messages": user(&gw.prompt("roll the dice")), "temperature": 0.9});

    gw.chat(&first, body.clone()).send().await.unwrap();
    gw.await_usage(&first).await;
    gw.chat(&second, body).send().await.unwrap();

    let row = gw.await_usage(&second).await;
    assert!(
        row.cache_hit.is_none(),
        "a non-reproducible request must not serve a stale answer"
    );
}

#[tokio::test]
async fn a_different_seed_is_a_different_cache_entry() {
    let gw: Gateway = gateway_or_skip!(true);
    let one = gw.request_id("seed-1");
    let two = gw.request_id("seed-2");

    let mut body = json!({
        "model": "fast", "messages": user(&gw.prompt("same prompt")),
        "temperature": 0.5, "seed": 1
    });
    gw.chat(&one, body.clone()).send().await.unwrap();
    gw.await_usage(&one).await;

    body["seed"] = json!(2);
    gw.chat(&two, body).send().await.unwrap();
    let row = gw.await_usage(&two).await;

    assert!(
        row.cache_hit.is_none(),
        "seed is part of the request; omitting it from the key collides two answers"
    );
    assert_eq!(row.cost_nano, 7650, "so the second one really did run");
}

// ── Fallback ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_fallback_is_named_in_a_header_and_billed_at_the_price_of_the_model_that_ran() {
    let gw: Gateway = gateway_or_skip!(false);
    let rid = gw.request_id("fallback");

    let resp = gw
        .chat(&rid, json!({"model": "flaky", "messages": user("hi")}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200, "the fallback rescued the request");
    assert_eq!(
        resp.headers()
            .get("x-switchboard-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("backup"),
        "the client must be able to tell which model answered"
    );

    let row = gw.await_usage(&rid).await;
    assert_eq!(row.model_alias, "flaky", "what was asked for");
    assert_eq!(row.served_by_alias, "backup", "what answered");
    // backup is $0.50/$1.00 per million: 11*500 + 10*1000. Billing `flaky`'s
    // $1.00/$2.00 would have charged 31_000 for tokens it never generated.
    assert_eq!(row.cost_nano, 15_500);
}

#[tokio::test]
async fn no_fallback_header_when_the_requested_model_answered() {
    let gw: Gateway = gateway_or_skip!(false);
    let rid = gw.request_id("no-fallback");

    let resp = gw
        .chat(&rid, json!({"model": "fast", "messages": user("hi")}))
        .send()
        .await
        .unwrap();

    assert!(resp.headers().get("x-switchboard-fallback").is_none());
    assert_eq!(gw.await_usage(&rid).await.served_by_alias, "fast");
}

#[tokio::test]
async fn a_dead_upstream_with_no_fallback_surfaces_as_an_error() {
    let gw: Gateway = gateway_or_skip!(false);
    let rid = gw.request_id("doomed");

    let resp = gw
        .chat(&rid, json!({"model": "doomed", "messages": user("hi")}))
        .send()
        .await
        .unwrap();

    // `503`, not `500`. A `500` says "this is a bug, do not retry", and `examples/bot-fleet`
    // reads it as `Outcome::Fatal` and abandons the request — while its AIMD controller,
    // which shrinks concurrency only on a `503`, never fires. A dead upstream would become a
    // permanent, silent, fleet-wide failure. docs/15 §7.
    assert_eq!(resp.status(), 503);

    // And it says when to come back, in both spellings. `Retry-After` is delta-seconds per
    // RFC 9110; `retry-after-ms` is the exact figure, and the one the SDKs prefer.
    assert_eq!(resp.headers()["retry-after"], "2");
    assert_eq!(resp.headers()["retry-after-ms"], "2000");
    // Which vendor failed. Otherwise a caller cannot tell "switchboard is broken" from
    // "Ollama is broken".
    assert_eq!(resp.headers()["x-switchboard-upstream"], "openai");

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "provider_unavailable");
}

// ── Budget ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_exhausted_monthly_budget_returns_429_insufficient_quota() {
    let gw: Gateway = gateway_or_skip!(false);
    let key = gw.tenant_with_budget(1_000, 5_000).await;

    let resp = gw
        .client
        .post(gw.url("/v1/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({"model": "fast", "messages": user("hi")}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 429);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "insufficient_quota");
}

#[tokio::test]
async fn a_tenant_within_its_budget_is_served() {
    let gw: Gateway = gateway_or_skip!(false);
    let key = gw.tenant_with_budget(1_000_000_000, 5_000).await;

    let resp = gw
        .client
        .post(gw.url("/v1/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({"model": "fast", "messages": user("hi")}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
}

// ── Idempotency ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_retried_request_with_the_same_idempotency_key_is_not_billed_twice() {
    let gw: Gateway = gateway_or_skip!(false);
    let key = format!("idem-{}", uuid::Uuid::now_v7());
    let first = gw.request_id("idem-1");
    let second = gw.request_id("idem-2");
    let body = json!({"model": "fast", "messages": user(&gw.prompt("charge me once"))});

    let r1 = gw
        .chat(&first, body.clone())
        .header("idempotency-key", &key)
        .send()
        .await
        .unwrap();
    assert_eq!(r1.status(), 200);
    let body1: serde_json::Value = r1.json().await.unwrap();
    assert_eq!(gw.await_usage(&first).await.cost_nano, 7650);

    let r2 = gw
        .chat(&second, body)
        .header("idempotency-key", &key)
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), 200);
    let body2: serde_json::Value = r2.json().await.unwrap();

    assert_eq!(
        body1["choices"][0]["message"]["content"], body2["choices"][0]["message"]["content"],
        "the retry must get the original answer"
    );

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(
        gw.usage_row_count(&second).await,
        0,
        "a replay runs no model and must not produce a second bill"
    );
}

#[tokio::test]
async fn reusing_an_idempotency_key_with_a_different_body_is_rejected() {
    let gw: Gateway = gateway_or_skip!(false);
    let key = format!("idem-{}", uuid::Uuid::now_v7());

    let r1 = gw
        .chat(
            &gw.request_id("idem-a"),
            json!({"model": "fast", "messages": user(&gw.prompt("question one"))}),
        )
        .header("idempotency-key", &key)
        .send()
        .await
        .unwrap();
    assert_eq!(r1.status(), 200);

    let r2 = gw
        .chat(
            &gw.request_id("idem-b"),
            json!({"model": "fast", "messages": user(&gw.prompt("a different question"))}),
        )
        .header("idempotency-key", &key)
        .send()
        .await
        .unwrap();

    // `409`, not `400`. Against a `400` a client cannot distinguish "your JSON is malformed"
    // (fix the request) from "you reused a key" (mint a new one), and those have opposite
    // remedies. Stripe's convention, and the IETF idempotency-key draft's. docs/15 §7.
    assert_eq!(
        r2.status(),
        409,
        "silently replaying the first answer would answer a question nobody asked"
    );
    let body: serde_json::Value = r2.json().await.unwrap();
    assert_eq!(body["error"]["type"], "idempotency_conflict");
    assert_eq!(body["error"]["param"], "Idempotency-Key");
}

#[tokio::test]
async fn different_idempotency_keys_run_independently() {
    let gw: Gateway = gateway_or_skip!(false);
    let first = gw.request_id("idem-x");
    let second = gw.request_id("idem-y");
    let body = json!({"model": "fast", "messages": user(&gw.prompt("run twice"))});

    gw.chat(&first, body.clone())
        .header("idempotency-key", format!("k1-{}", uuid::Uuid::now_v7()))
        .send()
        .await
        .unwrap();
    gw.chat(&second, body)
        .header("idempotency-key", format!("k2-{}", uuid::Uuid::now_v7()))
        .send()
        .await
        .unwrap();

    assert_eq!(gw.await_usage(&first).await.cost_nano, 7650);
    assert_eq!(gw.await_usage(&second).await.cost_nano, 7650);
}

#[tokio::test]
async fn a_failed_request_releases_its_idempotency_key() {
    let gw: Gateway = gateway_or_skip!(false);
    let key = format!("idem-{}", uuid::Uuid::now_v7());

    // `doomed` has no fallback and its upstream always 500s.
    let r1 = gw
        .chat(
            &gw.request_id("idem-fail"),
            json!({"model": "doomed", "messages": user("hi")}),
        )
        .header("idempotency-key", &key)
        .send()
        .await
        .unwrap();
    assert_eq!(r1.status(), 503, "a dead upstream is retryable, not a bug");

    // The guard's Drop clears the in-flight marker asynchronously.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let r2 = gw
        .chat(
            &gw.request_id("idem-retry"),
            json!({"model": "fast", "messages": user("hi")}),
        )
        .header("idempotency-key", &key)
        .send()
        .await
        .unwrap();

    assert_eq!(
        r2.status(),
        200,
        "a request that never completed must not lock its key for 24 hours"
    );
}

#[tokio::test]
async fn a_streamed_request_is_idempotent_too() {
    let gw: Gateway = gateway_or_skip!(false);
    let key = format!("idem-{}", uuid::Uuid::now_v7());
    let first = gw.request_id("idem-stream-1");
    let second = gw.request_id("idem-stream-2");
    let body = json!({
        "model": "fast", "messages": user(&gw.prompt("stream once")), "stream": true
    });

    let r1 = gw
        .chat(&first, body.clone())
        .header("idempotency-key", &key)
        .send()
        .await
        .unwrap();
    let _ = read_sse(r1).await;
    gw.await_usage(&first).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let r2 = gw
        .chat(&second, body)
        .header("idempotency-key", &key)
        .send()
        .await
        .unwrap();
    let replayed = read_sse(r2).await;

    assert!(replayed.contains(UPSTREAM_TEXT), "the replay is still SSE");
    assert!(replayed.ends_with("data: [DONE]\n\n"));

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(
        gw.usage_row_count(&second).await,
        0,
        "streaming retries double-bill just as badly as unary ones"
    );
}

// ── Failures reach the ledger ────────────────────────────────────────────────

#[tokio::test]
async fn a_failed_request_is_written_to_the_ledger_with_zero_cost() {
    let gw: Gateway = gateway_or_skip!(false);
    let rid = gw.request_id("error-row");

    let resp = gw
        .chat(&rid, json!({"model": "doomed", "messages": user("hi")}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);

    let row = gw.await_usage(&rid).await;
    assert_eq!(row.status, "error");
    assert_eq!(row.cost_nano, 0, "no tokens were generated");
    assert_eq!(row.completion_tokens, 0);
}

// ── OpenAI wire compatibility ────────────────────────────────────────────────

#[tokio::test]
async fn embeddings_accept_pre_tokenized_input() {
    let gw: Gateway = gateway_or_skip!(false);

    let resp = gw
        .client
        .post(gw.url("/v1/embeddings"))
        .json(&json!({"model": "embedder", "input": [[1, 2, 3]]}))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        200,
        "a client that already ran the tokenizer sends token ids"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["data"][0]["embedding"].is_array());
}

#[tokio::test]
async fn embeddings_can_return_base64() {
    let gw: Gateway = gateway_or_skip!(false);

    let resp = gw
        .client
        .post(gw.url("/v1/embeddings"))
        .json(&json!({"model": "embedder", "input": "hi", "encoding_format": "base64"}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let encoded = body["data"][0]["embedding"]
        .as_str()
        .expect("base64 is a string, not an array");

    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("valid base64");
    let floats: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(
        floats,
        vec![0.5, -0.25],
        "little-endian f32, as OpenAI sends"
    );
}

#[tokio::test]
async fn an_unknown_encoding_format_is_rejected_before_the_upstream_call() {
    let gw: Gateway = gateway_or_skip!(false);

    let resp = gw
        .client
        .post(gw.url("/v1/embeddings"))
        .json(&json!({"model": "embedder", "input": "hi", "encoding_format": "float16"}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["param"], "encoding_format");
}

#[tokio::test]
async fn an_unknown_model_is_a_400_with_model_not_found() {
    let gw: Gateway = gateway_or_skip!(false);

    let resp = gw
        .chat(
            &gw.request_id("unknown"),
            json!({"model": "no-such-model", "messages": user("hi")}),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "model_not_found");
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn the_clients_request_id_is_echoed_back() {
    let gw: Gateway = gateway_or_skip!(false);
    let rid = gw.request_id("echo");

    let resp = gw
        .chat(&rid, json!({"model": "fast", "messages": user("hi")}))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some(rid.as_str()),
        "correlating a client's log line with ours depends on this"
    );
}

#[tokio::test]
async fn the_legacy_completions_endpoint_returns_the_legacy_shape() {
    let gw: Gateway = gateway_or_skip!(false);

    let resp = gw
        .client
        .post(gw.url("/v1/completions"))
        .json(&json!({"model": "fast", "prompt": "hi"}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "text_completion");
    assert_eq!(body["choices"][0]["text"], UPSTREAM_TEXT);
}
