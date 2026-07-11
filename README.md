# 🔀 switchboard

**An OpenAI-compatible LLM gateway in Rust** — multi-provider routing, streaming, tool calling,
RAG, and per-tenant cost tracking.

> **Status: Phases 0–9 implemented.** RAG, observability, reliability, eval harness, and Docker deploy are in place.
>
> Two things the design docs describe are deliberately **not** built. The semantic
> cache is unimplemented because [docs/08](docs/08-reliability.md#4-caching) requires two weeks of shadow-mode
> false-hit data before it may serve traffic, and that data does not exist yet. An
> Anthropic provider is not built either — the trait and the anti-corruption layer
> are there for it, but no code claims support until there is code.

<!--
TODO: put a 10-second demo GIF right here, above the fold.
Show, in one take: tokens streaming in, an agent calling a tool, the cost dashboard ticking up.
This single asset does more work than the rest of the README combined. Record it with `vhs` or
`asciinema` + `agg`.
-->

---

## The idea

Point any OpenAI client at switchboard and it just works — but now you get provider routing,
fallbacks, caching, rate limiting, and cost accounting for free.

```python
from openai import OpenAI

client = OpenAI(base_url="http://localhost:8080/v1", api_key="sk-...")

client.chat.completions.create(
    model="qwen3:4b",        # served locally by Ollama
    messages=[{"role": "user", "content": "Analyze this transaction"}],
    stream=True,
)
```

Change `model` to `gpt-4o-mini` and the same call routes to OpenAI instead. No client changes.

Send an `Idempotency-Key` header and a retried request replays the original answer
instead of paying to generate a second one — the same key with a different body is
rejected with `409`, rather than silently answered from the first. It works on
`/v1/embeddings` too, which is the endpoint people actually retry.

Under overload the gateway **refuses rather than queues**: `503` with a `Retry-After`,
not a request parked until your client gives up. That refusal is not a degradation of
your p99 — it is the mechanism that produces one. Every response carries
`x-ratelimit-remaining-requests` and `-tokens`, so a bot fleet can throttle itself
before it finds its limit mid-sentence. See [docs/15](docs/15-latency-and-p99.md).

Measured, not asserted. At **10× rated capacity**, against an upstream whose latency the test controls:
90% of arrivals are shed in under 2 ms each, the rest are served at the gateway's rated throughput,
`/health` answers in **0.16 ms** p99 throughout, and memory does not grow. Gateway overhead is **4.0 ms**
p50 and **5.9 ms** p99. A provider killed mid-request produces zero `500`s — every one of them falls back.
`cargo run --release --bin loadtest -- --scenario all` reruns all of it, and prints the numbers on your
hardware rather than mine.

---

## Architecture

```
                    ┌─────────────────────────────────┐
  OpenAI SDK  ────► │  switchboard                    │
  (any client)      │                                 │
                    │  auth → rate limit → cache      │
                    │    → route → agent loop         │
                    │    → retry / fallback           │
                    │    → usage + cost accounting    │
                    └────┬──────────┬─────────────┬───┘
                         │          │             │
                    ┌────▼───┐ ┌────▼────┐  ┌─────▼─────┐
                    │ Ollama │ │ OpenAI  │  │   Tools   │
                    │ (local)│ │         │  │ MCP · SQL │
                    └────────┘ └─────────┘  └───────────┘

  Postgres (conversations · usage · pgvector)   Redis (cache · rate limit)
```

The full design — module layout, schema, provider abstraction, agent loop, RAG, eval — is in
[docs/](docs/README.md).

---

## Repository layout

```
crates/switchboard/    the product — one library, four bins (switchboard, sbctl, eval, loadtest)
examples/bot-fleet/    a third party using the product, over HTTP
docs/                  the design, 00–16
ops/                   Grafana dashboard, Prometheus alerts
config/                runtime config, read from the working directory
tests/conformance/     the real OpenAI Python SDK, against the built binary
```

`examples/bot-fleet/` is **excluded** from this Cargo workspace and has its own lockfile. `cargo test`
here never builds it, and it must never depend on the `switchboard` crate — it reaches the gateway over
HTTP, exactly as a customer does. That is the point of it, and
[docs/16](docs/16-example-bot-fleet.md) explains why an example that could reach inside the thing it
integrates with would be demonstrating the opposite of what it teaches.

---

## Roadmap

- [x] **Phase 0** — Docker Compose (Postgres + pgvector, Redis), CI, the Rust skeleton
- [x] **Phase 1** — OpenAI-compatible `/v1/chat/completions`, SSE streaming, `/v1/models`
- [x] **Phase 2** — `ModelProvider` trait; Ollama + OpenAI backends; model-based routing
- [x] **Phase 3** — API keys, per-tenant rate limiting, token & cost accounting
- [x] **Phase 4** — Tool calling, parallel tool calls, agent loop
- [x] **Phase 5** — RAG: `/v1/documents`, `/v1/search`, `/v1/embeddings`, hybrid retrieval
- [x] **Phase 6** — Prometheus metrics, request_id tracing, Grafana dashboard
- [x] **Phase 7** — Retry with backoff, circuit breaker, provider fallback, exact cache
- [x] **Phase 8** — Evaluation harness (`cargo run --bin eval`)
- [x] **Phase 9** — MCP client, Docker deploy
- [x] **Hardening** — load shedding, deadlines, per-provider concurrency caps, cached
  argon2, a batching usage sink; [docs/13](docs/13-stability-performance-plan.md) P0–P4
- [x] **Partner surface** — `/v1/keys` (create · rotate · revoke), per-end-user attribution via `user`,
  the rate card on `/v1/models`, budget headers and threshold webhooks
- [x] **The load test** — [docs/13 §P5](docs/13-stability-performance-plan.md#p5--measure-then-claim--done).
  `cargo run --release --bin loadtest -- --scenario all`. Every number in the SLO is now a measurement.
- [ ] **Per-tenant permit floors** — [docs/13 §1.6](docs/13-stability-performance-plan.md#16-fairness--whose-p99-is-it)
  mechanism 2. The load test measures a tenant with its rate limit raised refusing over 90% of another
  tenant's requests, without moving any latency metric this repository emits.

Every phase ships three things before the next one starts: something demoable, a test, and an entry
in this README.

---

## Benchmarks

Local eval captures latency vs machine-specific baselines in `eval/baselines/`. The profile is
detected from the CPU at run time; `EVAL_BASELINE_PROFILE` overrides it.

```bash
# Run eval + benchmark probes (search/chat/embed p95) against live server
cargo run --bin eval -- --suite fast --base-url http://localhost:8080 --benchmark

# Capture this machine as the baseline (e.g. apple-m4-metal)
cargo run --bin eval -- --suite fast --base-url http://localhost:8080 --benchmark --write-baseline
```

Measured on `apple-m4-metal` — MacBook Air (Mac16,12), Apple M4, 10 CPU cores (4P + 6E), 8 GPU cores,
16 GB unified memory, macOS 26.5.2 — with host Ollama 0.31.1 on Metal, serving `qwen3:4b` and
`nomic-embed-text`, built with rustc 1.96.0:

| probe | measured p95 | baseline | headroom |
|---|---:|---:|---:|
| search | 24ms | 600ms | −96% |
| chat | 437ms | 10s | −96% |
| embed | 18ms | 400ms | −96% |

Baselines are ceilings rather than targets, set high enough that the gate fires on a real regression
and not on a noisy afternoon:

| profile | search p95 | chat p95 | embed p95 | noise band |
|---|---:|---:|---:|---:|
| apple-m4-metal (M4, host Ollama) | 600ms | 10s | 400ms | ±30% |
| local (fallback, CPU or GPU) | 2s | 30s | 2s | ±50% |

Results written to `eval/results/<suite>-<timestamp>.json`. `--benchmark` exits non-zero when any
probe exceeds its baseline by more than the profile's noise band.

<!--
TODO: fill in accuracy numbers as eval suite grows. Examples of the shape:
  - hybrid search lifted recall@5 from 0.61 → 0.83, +12ms p95
  - qwen3:4b vs gpt-4o-mini on the tool-calling eval set: 0.71 vs 0.89 accuracy
-->

---

## RAG (Phase 5)

```bash
# Ingest a document (requires postgres + Ollama with nomic-embed-text)
curl -X POST localhost:8080/v1/documents \
  -H "Authorization: Bearer sk-..." \
  -d '{"title":"Guide","content":"EU refund policy: 30 days."}'

# Hybrid search (vector + full-text, fused with RRF)
curl -X POST localhost:8080/v1/search \
  -H "Authorization: Bearer sk-..." \
  -d '{"query":"refund policy EU","k":5,"mode":"hybrid"}'

# RAG-augmented chat — add rag field to chat request
curl -X POST localhost:8080/v1/chat/completions \
  -d '{"model":"qwen3:4b","rag":{"enabled":true,"top_k":3},"messages":[{"role":"user","content":"What is the refund policy?"}]}'
```

Search modes: `vector`, `lexical`, `hybrid` (default).

Chunking is structural and sized in tokens, not characters. Every chunk is prefixed
with its heading path (`# Guide > ## Auth > ### Rotation`), a table's header row is
repeated in each chunk it spills into, and code fences stay balanced. Retrieved text
reaches the model wrapped in `<untrusted_data>` and marks the request tainted, which
costs the agent loop its egress and side-effecting tools.

---

## Running locally

```bash
cp .env.example .env
docker compose up -d          # postgres + pgvector, redis
cargo run

# Full verification (auto: GPU→host Ollama, no GPU→docker CPU):
./run_local_test.sh
```

---

## Eval (Phase 8)

```bash
cargo run --bin eval -- --suite fast                                              # offline smoke
cargo run --bin eval -- --suite fast --base-url http://localhost:8080 --benchmark # online + latency
```

The `fast` suite is 7 cases in `eval/datasets/fast.jsonl` — three RAG retrieval, two tool calling,
two prompt injection — over the documents in `fast_seed.jsonl`, which the harness ingests first.

If the server at `--base-url` answers, the run scores online and the gate is the profile's
`min_pass_rate`. If it doesn't, the run degrades to an offline smoke test gated at 0.5, which is what
CI does on every PR. Authenticate with `--api-key` or `EVAL_API_KEY`.

---

## Observability (Phase 6)

- `GET /metrics` — Prometheus exposition format
- `x-request-id` on every response; `x-switchboard-fallback` names the model that
  actually answered when it wasn't the one requested
- `GET /v1/invocations/{request_id}` — which tools the agent called, and which were
  denied because untrusted content had entered the context
- Grafana dashboard: `ops/grafana/switchboard.json`
- Alert rules: `ops/prometheus/alerts.yml`

`sb_usage_records_dropped_total` is the one to page on. Every other metric describes
how the system is behaving; that one says a request was billed upstream and the row
recording it never landed.

Distributed tracing (OTLP, GenAI span conventions) is designed in
[docs/10 §2](docs/10-observability.md) and not yet built. Only the `http.request` span exists.

---

## License

MIT
