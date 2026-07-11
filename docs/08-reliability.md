# 08 — Reliability

← [07 RAG](07-rag.md) · [index](README.md) · next: [09 Evaluation](09-eval.md)

---

## 1. Retry — and when it is not safe

**Retry only before the first byte.** Once a single SSE delta has reached the client, a retry would
duplicate output; the request is no longer idempotent from the *client's* point of view even though it
still is from the provider's.

This is why `ModelProvider::stream` returns `Result<Stream, _>` rather than a stream of results
([02 §1](02-providers.md#1-the-trait)): the type system marks the boundary, so the unsafe retry is not
merely discouraged, it is unexpressible.

**Retry only on** `RateLimited | Overloaded | Upstream5xx | Timeout(Connect|Ttft)`. Full jitter, base
200ms, cap 8s, ≤2 attempts. Honour `Retry-After` when present, in preference to the backoff.

Two failure modes the naive version has:

- **Retry storms.** A retry budget — a token bucket permitting retries to be at most 10% of live
  traffic — means a provider brownout does not become a self-inflicted DDoS. When the budget is
  exhausted, the error returns immediately. This is the control that matters at 3am; the backoff
  curve is the one that gets discussed.
- **Retrying a tool loop.** Retrying is safe for a bare completion. It is *not* safe once the loop has
  executed a `SideEffecting` tool — the tool call already happened. Retries are therefore scoped to a
  single provider call *inside* the loop, never to the loop itself.

Two idempotency questions, two different answers, and conflating them is the bug:

| | idempotent? |
|---|---|
| Re-sending the same prompt to the provider | yes (you pay twice) |
| Re-running the agent loop that already sent an email | no |
| Re-sending after streaming 200 tokens to the client | no |

---

## 2. Circuit breaker

Per provider. Rolling 30s window; open at >50% error rate over ≥20 requests; half-open after 10s with
a single probe.

State lives **in process**. Each replica breaks independently, which is correct — the failure may be
local to one host's network path. State is *published* to `cb:{provider}` in Redis for observability
and so that a new replica starts warm.

A breaker that requires a Redis round trip on the hot path has added a dependency to the failure path
it exists to protect.

---

## 3. Fallback — and the semantics question, answered

`gpt-4o-mini` returns 429; `qwen3:4b` is available. Do we silently answer with a weaker model?

**No.** Silently degrading answer quality without telling the caller makes every downstream
measurement a lie — their eval scores, their A/B test, their incident post-mortem. The chosen
semantics:

- Fallback is **opt-in per model**, via `fallback = [...]` in the model registry.
- When it fires, the response's `model` field carries the model that **actually answered**. The OpenAI
  SDK exposes `response.model`; a caller who checks it sees the truth.
- `x-switchboard-fallback: gpt-4o-mini→qwen3:4b` is set on the response.
- `usage_records.model_alias` (requested) and `served_by_alias` (actual) are both stored, so "what
  fraction of last Tuesday's traffic was served by the fallback" is answerable.
- A tenant may set `allow_fallback = false` and receive the 429 instead.

Some callers would rather fail than be quietly downgraded, and they are usually right. A gateway that
makes that choice *for* them has taken a decision that was not its to take.

---

## 4. Caching

### Exact match

Key = `sha256(canonical_json(model, messages, tools, temperature, top_p, top_k, seed, max_tokens,
response_format, stop))`, prefixed by `tenant_id`.

Canonicalisation is the whole trick: sorted keys, no insignificant whitespace, absent distinguished
from null. A sloppy key is a cache that never hits; a *too*-sloppy key is a cache that returns the
wrong answer. `cache/key.rs` gets a `proptest`.

Only cache when the request is deterministic-ish (`temperature == 0` or `seed` present) and carries no
`SideEffecting` tools.

**Never cache across tenants.** Two tenants sending identical prompts is not a coincidence to exploit;
it is a leak waiting for a bug report.

Streaming clients get the cached response re-emitted as synthetic SSE chunks, so a hit and a miss are
indistinguishable on the wire. (A cache that only works for `stream: false` is a cache that never
fires in production.)

### Semantic — the dangerous one

*"What is the refund policy for EU customers?"* and *"What is the refund policy for US customers?"*
have cosine similarity ≈ 0.97 and completely different answers.

**A wrong semantic hit is strictly worse than no cache, because it is invisible.** No error, no log
line, no latency anomaly. Just a wrong answer, served fast.

Therefore:

- An entry is eligible only when the conversation **prefix hash** and the **params hash** match
  exactly. Only the final user turn is compared by embedding.
- Threshold τ starts at 0.95, and τ is measured, not chosen.
- It ships in **shadow mode first**. For two weeks the gateway records what it *would have* served,
  scores that against what it actually served, and only then is the cache enabled — with the measured
  false-hit rate published.

"34% hit rate, 0.4% false-hit rate, p95 1.2s → 380ms" is a claim. "Has a semantic cache" is not.

---

## 5. Four timeouts, not one

| clock | default | what it catches |
|---|---|---|
| `connect` | 2s | the provider's socket is gone |
| `ttft` | 30s | prefill is slow, or Ollama is loading a model from disk |
| `stall` | 30s | the stream started and then stopped producing |
| `total` | 300s | a long, legitimate generation that has to end sometime |

One global timeout cannot distinguish "the provider is down" from "the model is thinking", and those
need different responses. A 30s `ttft` on a cold Ollama load is not a failure; a 30s gap between
deltas is.

`connect` and `ttft` are the two of the four whose expiry is retryable, because they are the two that fire
before a byte has reached the client. `stall` and `total` fire after tokens have been generated and paid
for, so retrying them buys one answer twice. `reliability::is_retryable` is the single place this is
decided, and it names both.
