# 12 — Build order

← [11 Config, deploy, testing](11-config-deploy-testing.md) · [index](README.md) · next: [13 Stability & performance plan](13-stability-performance-plan.md)

Everything is *designed* at once. Nothing is *built* at once.

---

## The dependency DAG

Parts on the same line are independent and can proceed in parallel.

```
domain/ + config/ + http/error + telemetry/logs      ← nothing else compiles without these
        │
  providers/{trait, ollama} + http/sse                ← Phase 1 ends when the Python SDK passes
        │
  ┌─────┴──────────────┬───────────────────┐
providers/openai   db/ + auth/ + usage/   rag/embed    (three independent tracks)
  │                    │                    │
registry + routing  ratelimit/ + budget   rag/{chunk,retrieve}
  │                    │                    │
  └─────┬──────────────┴──────────┬─────────┘
   agent/{react,tools}       cache/ + reliability/
        │                         │
        └───────────┬─────────────┘
              telemetry/{traces,metrics}
                    │
                eval/  ← needs everything, and judges everything
                    │
              mcp + deploy + load test
```

---

## Mapping to the roadmap's phases

| roadmap phase | documents that describe it |
|---|---|
| 0 — Foundations | [11](11-config-deploy-testing.md) |
| 1 — Passthrough ★ | [01](01-backend.md), [02 §1](02-providers.md#1-the-trait) |
| 2 — Multi-provider | [02](02-providers.md) |
| 3 — Auth, rate limit, cost | [03](03-database.md), [04](04-redis-ratelimit.md), [05](05-tokenization-cost.md) |
| 4 — Tool calling | [06](06-agent-tools.md) |
| 5 — RAG | [07](07-rag.md) |
| 6 — Observability | [10](10-observability.md) |
| 7 — Reliability | [08](08-reliability.md) |
| 8 — Evaluation ★ | [09](09-eval.md) |
| 9 — MCP, deploy | [06 §5](06-agent-tools.md#5-mcp), [11 §2](11-config-deploy-testing.md#2-deploy) |

---

## Three rules the DAG does not show

**Phase 1 is a gate, not a step.** The Python `openai` SDK must pass against Ollama, streaming and
non-streaming, before anything on the second line of the DAG begins. Everything downstream assumes the
wire format is correct; discovering it is not, three months later, invalidates every measurement taken
in between.

**The roadmap's invariant holds unchanged.** Each box ships three things — something demoable, a test,
and a README entry — or the next box does not start. Missing any of the three means the box isn't done.

**Write the README section as each box lands, not at the end.** The post *"I built an OpenAI-compatible
gateway in Rust, and here's what the spec doesn't tell you"* is worth half the repo, and it is
unwritable from memory six months later. The specific things worth writing down as they happen:

- What the OpenAI streaming spec does not say, and what the SDK enforces anyway.
- What actually happened to the upstream request when the client disconnected — measured, with a
  packet capture, not assumed.
- The first time the agent loop looped forever, and which of the four termination conditions was
  missing.
- The first prompt injection that worked, verbatim, and what the taint rule did to it afterwards.
- The number the semantic cache produced in shadow mode, including if that number said not to ship it.

Those five paragraphs are the ones nobody else can write, because they require having built the thing.
