# 05 — Tokenization, budgets, and cost

← [04 Redis & rate limiting](04-redis-ratelimit.md) · [index](README.md) · next: [06 Agent & tools](06-agent-tools.md)

The whole reason tokenization appears in a backend project: **there is no enforcing a rate limit or a
cost cap without counting tokens before the response exists.**

---

## 1. One tokenizer per model, cached

`usage::counter` holds an `Arc<dyn Tokenizer>` per model alias, built once and kept in a `moka` cache:

| model family | implementation |
|---|---|
| OpenAI | `tiktoken-rs`, encoding named in the model registry (`o200k_base`) |
| Ollama / HF | `tokenizers`, `tokenizer.json` fetched once and **vendored into the image** |
| unknown | `chars / 3.6`, and the record is flagged `estimated = true` |

Vendoring the HF tokenizer is not fussiness. A gateway that cannot start without network access to
HuggingFace is a gateway that cannot start.

The unknown case never silently pretends. A number whose provenance is not recorded is a number that
will be trusted by someone later.

---

## 2. Counting a request is not counting a string

The prompt the model sees is not the JSON the client sent. Between them sit:

- **The chat template.** Role markers, `<|im_start|>`-style delimiters, a system prompt the client
  never wrote. Different per model.
- **Tool schemas**, serialised into the prompt in a model-specific way — often as JSON, sometimes as
  TypeScript-ish signatures, and the difference is hundreds of tokens.
- **Images**, which cost tokens by tile count, not by byte count.

So `count_request()` returns a range, not a number:

```rust
pub struct TokenEstimate { pub lower: u32, pub expected: u32, pub upper: u32 }
```

The budget check uses `upper`. The billed number always comes from the response.

**Estimate to enforce, measure to bill.** Six words that resolve most of the arguments this subject
produces.

---

## 3. Why `temperature = 0` is not a spec of determinism

Worth stating here because it is the reason the eval gate in [09](09-eval.md) has a noise band, and
the reason the exact-match cache in [08 §4](08-reliability.md#4-caching) is a heuristic and not a
guarantee.

`temperature = 0` makes sampling an argmax. It does not make the logits identical run to run: batching
changes the order of floating-point reductions on the GPU, so the last bits of each logit differ, so
an argmax tie flips. Continuous batching means the batch composition depends on *other people's
traffic*.

Identical input, identical seed, different output. Design around it rather than being surprised by it.

---

## 4. Prices and cost

The price table is config, not code ([02 §3](02-providers.md#3-routing)), because a price change
should not need a rebuild.

```rust
// price_in / price_out are USD per 1M tokens, as the vendors quote them.
fn cost_nano(spec: &ModelSpec, u: &Usage) -> i64 {
    let nano_per_usd = 1_000_000_000i128;
    let inp = (u.prompt_tokens as i128) * (spec.price_in_micros as i128);   // µUSD per 1M
    let out = (u.completion_tokens as i128) * (spec.price_out_micros as i128);
    (((inp + out) * nano_per_usd) / (1_000_000 * 1_000_000)) as i64
}
```

Prices are parsed from TOML into fixed-point micro-USD-per-million at boot, and every arithmetic step
after that is integer. There is no `f64` anywhere between the config file and `usage_records.cost_nano`.

Cached prompt tokens (`cached_tokens`) and reasoning tokens (`reasoning_tokens`) are priced
separately by the vendors that expose them, and the columns exist in the schema precisely so that the
day they matter is a config change and not a migration.

---

## 5. Budget enforcement

```
budget:{tenant}:{yyyymm}  (Redis, O(1))
    └─ miss ─→ rebuild from usage_hourly  ─→ cache
```

Over budget → `429` with `type: insufficient_quota`. That is the code the OpenAI SDK maps to its
`RateLimitError` subclass, so a client that already handles OpenAI's quota errors handles ours without
a line of new code. Matching the *error taxonomy* is part of being OpenAI-compatible, and it is the
part everybody forgets.

Postgres remains the ledger. Redis is a counter that is allowed to be slightly stale and is never
allowed to be the reason an invoice is wrong.

---

## 6. The three numbers on every record

Restating the settlement from [01 §5](01-backend.md#5-cancellation-and-backpressure), because it is a
billing decision and it belongs in this document too:

| situation | `prompt_tokens` | `completion_tokens` | `status` |
|---|---|---|---|
| normal completion | from upstream usage | from upstream usage | `completed` |
| client disconnects mid-stream | full | tokens that crossed the wire, counted locally | `client_disconnect` |
| upstream error before first byte | 0 | 0 | `error` |
| cache hit | 0 | 0 | `completed`, `cache_hit = exact` |

The `status` column keeps the four populations separable. A p95 latency that silently averages
completed requests with abandoned ones is a metric that means nothing.
