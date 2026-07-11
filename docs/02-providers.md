# 02 — Providers

← [01 Backend](01-backend.md) · [index](README.md) · next: [03 Database](03-database.md)

The anti-corruption layer. This is the largest architectural lesson in the project, so it gets its
own document.

---

## 1. The trait

```rust
#[async_trait]
pub trait ModelProvider: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn capabilities(&self, model: &str) -> Caps;

    async fn chat(&self, req: &ChatRequest, cx: &CallCx) -> Result<ChatResponse, ProviderError>;
    async fn stream(&self, req: &ChatRequest, cx: &CallCx)
        -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;
    async fn embed(&self, req: &EmbedRequest, cx: &CallCx) -> Result<EmbedResponse, ProviderError>;
}

pub struct Caps {
    pub tools: bool, pub parallel_tool_calls: bool, pub json_schema: bool,
    pub vision: bool, pub streaming_usage: bool, pub max_context: u32,
}
```

Note what `stream` returns: `Result<Stream, _>`, not `Stream<Item = Result<_>>` alone. The outer
`Result` resolves **before the first byte**, which is what makes the retry policy in
[08 §1](08-reliability.md#1-retry--and-when-it-is-not-safe) expressible in the type system: a failure
in the outer `Result` is retryable, a failure inside the stream is not.

`Caps` is consulted at the boundary, not deep inside. A request with `tools` against a model whose
`Caps.tools == false` fails at the router with a 400 naming the model — it does not fail three layers
down with a confusing upstream 500.

---

## 2. Where normalisation happens, and why there

Each `providers/<vendor>/` has exactly three files:

- **`wire.rs`** — `#[derive(Serialize, Deserialize)]` structs mirroring the vendor's JSON *exactly*,
  including their bad names. No `serde(rename)` gymnastics to make them look like ours.
- **`mapping.rs`** — `From<&domain::ChatRequest> for wire::Request` and
  `TryFrom<wire::Chunk> for domain::StreamEvent`. This file *is* the anti-corruption layer. It is the
  only file that is allowed to be ugly.
- **`mod.rs`** — the `ModelProvider` impl: HTTP, auth headers, error classification. Thin.

**Why at the vendor boundary and not in the handler:** because the number of vendors grows and the
number of handlers does too. Mapping in the handler is O(vendors × handlers) mappings; mapping at the
boundary is O(vendors). The agent loop, the cache, the token counter, the usage recorder and the eval
harness all consume `domain::` types and none of them changed when Anthropic was added.

That is the entire claim, and it is falsifiable: **adding a fourth provider must touch exactly one
directory.**

### The divergence that earns the layer its keep

Streaming tool calls, three vendors, three shapes:

| | tool-call streaming shape |
|---|---|
| OpenAI | `delta.tool_calls[i]` with `index`, `id` on the first fragment only, `function.arguments` as partial-JSON text |
| Anthropic | `content_block_start{type:tool_use}` → N × `input_json_delta{partial_json}` → `content_block_stop` |
| Ollama | one message with a complete `tool_calls[]`, no ids, no fragments |

All three become `StreamEvent::ToolCallDelta`. Accumulation into a whole `ToolCall` happens **once**,
in `agent/react.rs`, over the normalised events — not three times, once per vendor.

---

## 3. Routing

`config/models.toml` is the single source of truth. Nothing about a model is hardcoded:

```toml
[[model]]
alias          = "qwen3:4b"
provider       = "ollama"
upstream_name  = "qwen3:4b"
context        = 32768
price_in       = 0.0        # USD per 1M prompt tokens
price_out      = 0.0
tokenizer      = { kind = "hf", repo = "Qwen/Qwen3-4B" }

[[model]]
alias          = "gpt-4o-mini"
provider       = "openai"
upstream_name  = "gpt-4o-mini"
context        = 128000
price_in       = 0.15
price_out      = 0.60
tokenizer      = { kind = "tiktoken", encoding = "o200k_base" }
fallback       = ["qwen3:4b"]       # opt-in, and never silent — see 08 §3
```

Routing is a map lookup on `alias`. An unknown alias returns 404 with OpenAI's `model_not_found` code,
because that is what the SDK's error handling expects.

`GET /v1/models` is generated from this file, so it can never drift from what actually routes.

---

## 4. Error taxonomy

The retry policy is a function of this enum and nothing else:

```rust
pub enum ProviderError {
    RateLimited { retry_after: Option<Duration> },   // retryable
    Overloaded,                                      // retryable (Anthropic 529, OpenAI 503)
    Timeout(TimeoutKind),                            // retryable iff Connect | Ttft
    Upstream5xx { status: u16 },                     // retryable
    Auth,                                            // not — a key does not fix itself
    BadRequest { param: Option<String> },            // not
    ContextLength { limit: u32, got: u32 },          // not — retrying sends the same tokens
    ContentFilter,                                   // not
    Cancelled,                                       // not — the client is gone
    Decode(serde_json::Error),                       // not — our bug, and it will recur
}
```

Classification happens in each provider's `mod.rs`, because only there is it known that Anthropic's
529 means "overloaded" while OpenAI's 503 does. Everything above the line branches on the enum.

---

## 5. `mock.rs` — the fourth provider

A cassette player: golden SSE transcripts recorded once from real OpenAI, replayed byte-for-byte. It
makes CI deterministic and free, it is the fixture for the conformance tests, and it is the fast eval
suite's backend. It also serves as the honest test of the trait — a provider that reads from disk
should need no special case anywhere.
