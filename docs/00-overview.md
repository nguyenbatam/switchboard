# 00 — Overview

← [index](README.md) · next: [01 Backend](01-backend.md)

---

## Shape of the whole thing

```
                         ┌──────────────────────────────────────────────┐
   OpenAI SDK ─────────► │ http/          axum, SSE, OpenAI error shape │
   (any client)          ├──────────────────────────────────────────────┤
                         │ auth/          key hash → Tenant             │
                         │ ratelimit/     Redis GCRA, req/min + tok/min │
                         │ usage/budget   pre-flight token estimate     │
                         │ cache/         exact → semantic              │
                         ├──────────────────────────────────────────────┤
                         │ domain/        the ONE internal shape        │◄── anti-corruption line
                         ├──────────────────────────────────────────────┤
                         │ agent/         loop · tools · taint tracking │
                         │ rag/           chunk · embed · hybrid search │
                         │ reliability/   retry · breaker · fallback    │
                         │ providers/     trait + per-vendor mapping    │
                         └───┬────────────┬───────────────┬─────────────┘
                             │            │               │
                        ┌────▼───┐  ┌─────▼────┐   ┌──────▼──────┐
                        │ Ollama │  │ OpenAI   │   │ Tools       │
                        │        │  │ Anthropic│   │ MCP · SQL   │
                        └────────┘  └──────────┘   └─────────────┘

   Postgres: tenants · keys · usage · conversations · pgvector · semantic cache · eval
   Redis:    rate-limit buckets · exact-match cache · idempotency · breaker gossip
```

---

## The two lines that carry the design

### 1. The anti-corruption line

Nothing above it knows a vendor exists. Nothing below it knows what a tenant is. Every vendor quirk
dies inside `providers/<vendor>/mapping.rs`.

The claim is falsifiable, which is the point: **adding a fourth provider must touch exactly one
directory.** The agent loop, the cache, the token counter, the usage recorder and the eval harness
all consume `domain::` types, and none of them changed when Anthropic was added.

Detail in [02 — Providers](02-providers.md).

### 2. The taint line

Once untrusted bytes — a RAG chunk, a SQL row, an MCP tool result — enter the context, the request's
tool capabilities are **downgraded**, not merely logged.

The observation this rests on: *the model cannot distinguish instructions from data, and it never
will.* Any design that relies on the model ignoring an instruction embedded in a document is not a
defense; it is a hope. So instead of trying to make the untrusted text safe, make the privileges
safe.

Detail in [06 §4 — Prompt injection](06-agent-tools.md#4-prompt-injection-taint-and-capability-downgrade).

---

## Decisions inherited from the roadmap, not reopened

| decision | choice |
|---|---|
| Language | Rust, a single binary |
| Vector store | pgvector, not Qdrant |
| Kubernetes | Deferred, probably forever |
| Dev model | Ollama + Qwen3 4B / 8B; $20 of OpenAI for compatibility testing |
| Tool count | 3 tools, deep |

One addition these documents make: **one crate, three bin targets** (`switchboard`, `sbctl`, `eval`).
Still a single deployable binary; the eval harness and the key-admin CLI need the library, not a copy
of it.

---

## Money, time, and ids — three conventions set once

- **Money is `bigint` nanodollars (1e-9 USD).** Never `f64`. `gpt-4o-mini` input is
  $0.00000015/token; in floating point that accumulates visible error over a month of traffic, and
  "visible error in the billing table" is not a sentence anyone wants to write.
- **Timestamps are `timestamptz`.** Always. No exceptions, no `timestamp`.
- **Ids are UUIDv7.** Time-ordered, so they index like a bigint and sort like a timestamp, without
  leaking a sequence count to whoever holds one.
