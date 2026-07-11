# 01 — Backend

← [00 Overview](00-overview.md) · [index](README.md) · next: [02 Providers](02-providers.md)

---

## 1. Crate and module layout

One binary, as locked in the roadmap. But one crate with three bin targets, because the eval harness
and the key-admin CLI need the library, not a copy of it.

```
Cargo.toml                    # [lib] switchboard + 3 [[bin]]
config/
  switchboard.toml            # committed defaults
  models.toml                 # model registry + price table
src/
  main.rs                     # bin: the gateway
  bin/
    sbctl.rs                  # bin: create/revoke keys, backfill rollups
    eval.rs                   # bin: the eval harness (CI runs this)
  lib.rs

  config/
    mod.rs                    # figment: toml ← env (SB__SERVER__PORT), typed, validated at boot
    models.rs                 # ModelSpec: alias, provider, upstream_name, caps, price, fallback
    pricing.rs                # Money: nanodollars, never f64

  http/
    mod.rs                    # Router assembly, tower layer order (§2)
    error.rs                  # ApiError → OpenAI envelope {error:{message,type,param,code}}
    sse.rs                    # SSE encoder: `data: {json}\n\n` … `data: [DONE]\n\n`
    extract.rs                # Tenant, RequestId, TypedJson (400 that looks like OpenAI's 400)
    routes/
      chat.rs                 # POST /v1/chat/completions
      completions.rs          # POST /v1/completions   (legacy; thin shim onto chat)
      models.rs               # GET  /v1/models, GET /v1/models/{id}
      embeddings.rs           # POST /v1/embeddings
      documents.rs            # POST /v1/documents, GET/DELETE /v1/documents/{id}
      search.rs               # POST /v1/search        (non-OpenAI, ours)
      usage.rs                # GET  /v1/usage (totals + balance), /v1/usage/records (history)
      ops.rs                  # GET  /health, /ready, /metrics

  domain/                     # provider-neutral. no serde attrs from any vendor here.
    chat.rs                   # ChatRequest, Message, Content, ToolCall, FinishReason
    stream.rs                 # StreamEvent (§4)
    embed.rs
    usage.rs                  # Usage { prompt, completion, cached, reasoning }
    tenant.rs

  auth/
    mod.rs                    # from_bearer() → Tenant
    key.rs                    # generate / hash / verify. sk-sb-<prefix>.<secret>

  ratelimit/
    mod.rs                    # RateLimiter trait
    gcra.rs                   # the Lua script + its explanation

  providers/
    mod.rs                    # trait ModelProvider, ProviderError, Caps
    registry.rs               # alias → Arc<dyn ModelProvider> + fallback chain
    ollama/{mod.rs,wire.rs,mapping.rs}
    openai/{mod.rs,wire.rs,mapping.rs}
    anthropic/{mod.rs,wire.rs,mapping.rs}
    mock.rs                   # cassette replay: deterministic CI, zero dollars

  agent/
    react.rs                  # the ReAct loop + its four termination conditions.
                              # Not loop.rs: `mod loop` does not parse — `loop` is a keyword.
    budget.rs                 # iterations, tokens, wall-clock deadline
    registry.rs               # ToolSpec { schema, trust, side_effects, timeout }
    taint.rs                  # capability downgrade after untrusted input
    tools/
      calculator.rs
      sql_query.rs            # read-only role + AST allowlist
      wallet_balance.rs       # fixed RPC egress, retries, unpredictable latency
      mcp.rs                  # rmcp client: remote tools adopted into the registry

  rag/
    ingest.rs                 # parse → normalise → chunk → dedupe → embed → store
    chunk.rs                  # recursive structural chunker
    embed.rs                  # batching, task prefixes, dimension pinning
    retrieve.rs               # vector + FTS → RRF → MMR → order
    context.rs                # assembly, delimiting, citation ids

  cache/
    exact.rs                  # Redis, sha256 of a canonicalised request
    semantic.rs               # pgvector, shadow-mode first
    key.rs                    # canonicalisation — the part that is easy to get wrong

  reliability/
    retry.rs                  # pre-first-byte only, full jitter, retry budget
    breaker.rs                # per-provider, rolling window
    fallback.rs               # explicit, never silent
    timeout.rs                # connect / ttft / stall / total — four different clocks

  usage/
    counter.rs                # tiktoken-rs | tokenizers, per-model, cached
    recorder.rs               # bounded mpsc → batching writer task
    price.rs

  db/
    mod.rs                    # PgPool (app) + PgPool (tool_ro, different role)
    tenants.rs keys.rs usage.rs documents.rs chunks.rs conversations.rs eval.rs
  migrations/                 # sqlx, forward-only, applied under an advisory lock

  telemetry/
    logs.rs metrics.rs traces.rs

  eval/
    dataset.rs runner.rs judge.rs metrics.rs report.rs

tests/
  conformance/                # pytest, the real `openai` SDK, against the real binary
  golden/                     # SSE transcripts recorded once from real OpenAI
```

### Dependencies worth naming

`axum` · `tokio` · `tower` + `tower-http` · `reqwest` (stream) · `eventsource-stream` · `serde` ·
`sqlx` (pg, uuid, chrono, macros) · `deadpool-redis` · `figment` · `tracing` +
`tracing-opentelemetry` + `opentelemetry-otlp` · `metrics-exporter-prometheus` · `tiktoken-rs` ·
`tokenizers` · `jsonschema` · `sqlparser` · `argon2` · `sha2` · `uuid` (v7) · `moka` · `rmcp` ·
`thiserror` · `async-trait` · `alloy-primitives` (`U256` and EIP-55, for `wallet_balance`).

Dev: `proptest` (the SSE codec and the cache canonicaliser) · `wiremock` (provider contract tests).

`async-trait` earns its place despite native async-in-trait being stable: the registry holds
`Arc<dyn ModelProvider>`, and a trait with `async fn` is not dyn-safe.

---

## 2. The request pipeline, in order

Order is load-bearing. Each step is cheap relative to the one after it, which is the whole reason for
the ordering:

```
  request_id  →  trace span  →  auth  →  rate limit: requests  →  cache lookup
      →  token count  →  rate limit: tokens (reserve)  →  budget check
      →  route  →  [agent loop | direct call]  →  stream
      →  rate limit: tokens (settle)  →  usage record (on completion OR on drop)
```

- **Auth before rate limit** — the limit is per tenant, so the tenant has to exist first. An
  unauthenticated flood is absorbed by a separate global IP bucket in front of everything.
- **The rate limiter is two steps, not one, and they sit on opposite sides of the cache.** The
  request bucket (`rl:req`, cost 1) is checkable immediately and gates everything after it: a Redis
  `GET` for the cache is not free, and an over-limit tenant has no right to one. The token bucket
  (`rl:tok`) *cannot* run here, because its cost is `estimated_prompt_tokens + max_tokens` and the
  estimate does not exist until the token count two steps later. Reserving before the cache would
  also charge a cache hit for tokens no upstream ever generated. See
  [04 §4](04-redis-ratelimit.md#4-reserve-and-settle-the-token-bucket-is-called-twice).
- **Cache before the token count** — counting tokens with BPE over the whole prompt is the most
  expensive thing in this list. A cache hit skips the tokenizer entirely, which is exactly why the
  cache sits above it.
- **Token count before budget check** — the budget is denominated in tokens and dollars, so there is
  nothing to check until the count exists. See [05](05-tokenization-cost.md).
- **Usage record last, and unconditionally.** It is emitted from a `Drop` impl, not from the happy
  path. If it lived on the happy path, a client disconnect would bill nothing. The token-bucket
  settle rides the same `Drop`, for the same reason.

Tower layers, outermost first: `TraceLayer` → `RequestIdLayer` → `CatchPanic` → `Cors` →
`ConcurrencyLimit` → `Timeout(total)` → routes. Auth and rate limit are `from_fn` middleware inside
the `/v1` nest, not global, so `/health` stays reachable when Redis is down.

---

## 3. Errors

One enum, `ApiError`, and one rendering function. Every error a client sees is shaped like OpenAI's,
because clients parse it:

```json
{"error":{"message":"…","type":"invalid_request_error","param":"messages[0].role","code":null}}
```

`type` ∈ `invalid_request_error | authentication_error | permission_error | rate_limit_error |
server_error | insufficient_quota`.

Upstream provider errors are **translated, never proxied** — a raw Anthropic error body reaching an
OpenAI client is a compatibility bug. Retryable upstream failures that exhaust their retries become
`server_error`, with `x-switchboard-upstream: anthropic` on the response for debugging.

---

## 4. The streaming contract

The internal event type. Everything the providers emit is normalised into this before the SSE layer
sees it, and the SSE layer is the only thing that knows OpenAI's chunk shape:

```rust
pub enum StreamEvent {
    Start          { id: String, model: String, created: i64 },
    Delta          { content: String },
    ReasoningDelta { content: String },        // o-series, qwen3 <think>; dropped unless requested
    ToolCallDelta  { index: u32, id: Option<String>, name: Option<String>, arguments: String },
    Finish         { reason: FinishReason },
    Usage          (Usage),                    // emitted iff stream_options.include_usage
}
```

`ToolCallDelta.arguments` is a **fragment of a JSON string**, not JSON. OpenAI streams tool arguments
as partial text and so must we. Ollama does not — it emits one complete tool call — so
`ollama/mapping.rs` synthesises an id (`call_` + uuid), emits a single delta carrying the whole
argument string, and the client cannot tell. That asymmetry is exactly what the anti-corruption layer
is for.

`finish_reason` mapping is a table, not an if-chain:

| internal | OpenAI | Anthropic | Ollama |
|---|---|---|---|
| `Stop` | `stop` | `end_turn`, `stop_sequence` | `done_reason: stop` |
| `Length` | `length` | `max_tokens` | `done_reason: length` |
| `ToolCalls` | `tool_calls` | `tool_use` | non-empty `tool_calls` |
| `ContentFilter` | `content_filter` | `refusal` | — |

---

## 5. Cancellation and backpressure

The two questions from the roadmap, answered structurally.

### Backpressure

Between the provider-reading task and the axum response body sits a `mpsc::channel(32)`. When the
client stops reading, the OS socket buffer fills, axum stops polling the body, the channel fills, the
reader task stops polling `reqwest`, its TCP window closes, and the upstream slows down.

Backpressure propagates end-to-end for free **provided nothing in the chain is unbounded**. There is
exactly one bounded channel and no `Vec` accumulating chunks. A `stall_timeout` (no progress for 30s)
cancels the whole thing, so a hung client cannot pin an upstream connection forever.

### Cancellation

When the client disconnects, axum drops the response body. Dropping the body drops the receiver; the
sender's `send` fails; the reader task observes the failure and drops the `reqwest` response, which
closes the upstream connection. Ollama sees the close and stops generating. OpenAI sees the close and
stops billing at the next token boundary.

None of that is automatic. It works only because the reader task's error path is `return`, not
`continue`.

### Billing on disconnect

A `StreamGuard` owns the counter and the sink:

```rust
struct StreamGuard { started: Instant, ttft: Option<Duration>, counted: TokenCounter,
                     sink: UsageSink, cancel: CancellationToken, finished: bool }

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.cancel.cancel();                       // fan out to reader + tool tasks
        self.sink.emit(UsageRecord {                // non-blocking; bounded queue
            completion_tokens: self.counted.total(),
            status: if self.finished { Completed } else { ClientDisconnect },
            ..
        });
    }
}
```

**The policy, stated so nobody has to guess:** bill `prompt_tokens` in full and `completion_tokens`
for every token that crossed the wire, because that is what the upstream charged us for. Tokens
generated upstream after our disconnect and before its next boundary are eaten by us — a rounding
error, and the alternative (billing tokens the client never saw) is worse.

The record carries `status = client_disconnect`, so the two populations are separable in analysis and
neither is silently mixed into p95 latency.

Completion tokens are counted **locally**, from the deltas, as they arrive. The upstream `usage` chunk
is used to *reconcile* when it arrives (`stream_options: {include_usage: true}`); a mismatch beyond 2%
increments `sb_token_count_drift_total` and the upstream number wins. On disconnect there is no
upstream chunk at all, which is precisely why the local counter has to exist.

### The usage sink

A bounded `mpsc` into a background writer task that batches inserts (500 rows or 200ms, whichever
first). The request path never blocks on Postgres. If the queue is full, the writer has been down
long enough that something else is already paging: emit to the log, increment
`sb_usage_records_dropped_total`, and keep serving. Billing data is important; refusing to serve
traffic because the billing table is slow is worse.
