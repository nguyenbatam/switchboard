# 🔀 switchboard

**An OpenAI-compatible LLM gateway in Rust** — multi-provider routing, streaming, tool calling,
RAG, and per-tenant cost tracking.

> ⚠️ **Status: early development (Phase 0).** Nothing below is implemented yet. This README is the
> target. The checkboxes get ticked as things land, and this notice gets replaced with real numbers.

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
                    │ (local)│ │ Claude  │  │ MCP · SQL │
                    └────────┘ └─────────┘  └───────────┘

  Postgres (conversations · usage · pgvector)   Redis (cache · rate limit)
```

---

## Roadmap

- [ ] **Phase 1** — OpenAI-compatible `/v1/chat/completions`, SSE streaming, `/v1/models`
- [ ] **Phase 2** — `ModelProvider` trait; Ollama + OpenAI backends; model-based routing
- [ ] **Phase 3** — API keys, per-tenant rate limiting, token & cost accounting
- [ ] **Phase 4** — Tool calling, parallel tool calls, agent loop
- [ ] **Phase 5** — RAG: `/v1/documents`, `/v1/search`, `/v1/embeddings`, hybrid retrieval
- [ ] **Phase 6** — OpenTelemetry traces, Prometheus metrics, Grafana dashboard
- [ ] **Phase 7** — Retry with backoff, circuit breaker, provider fallback, response cache
- [ ] **Phase 8** — Evaluation harness running in CI
- [ ] **Phase 9** — MCP client, deploy, load test

Every phase ships three things before the next one starts: something demoable, a test, and an entry
in this README.

---

## Benchmarks

<!--
TODO: fill this in as Phase 8 lands. Real numbers only. Examples of the shape:
  - hybrid search lifted recall@5 from 0.61 → 0.83, +12ms p95
  - exact-match cache: 34% hit rate, p95 1.2s → 380ms
  - qwen3:4b vs gpt-4o-mini on the tool-calling eval set: 0.71 vs 0.89 accuracy
Never write "supports RAG". Write the number.
-->

_Coming with Phase 8._

---

## Running locally

```bash
ollama serve && ollama pull qwen3:4b && ollama pull nomic-embed-text
cp .env.example .env
docker compose up -d          # postgres + pgvector, redis
cargo run
```

---

## License

MIT
