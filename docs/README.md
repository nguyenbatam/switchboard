# switchboard — architecture

The end-state design, written as if all nine roadmap phases ship at once. The
[README's roadmap](../README.md#roadmap) says *when*; these documents say *what* and *why*. Where a
decision could reasonably have gone the other way, the reason is stated inline — that reason is the
only thing that stops it from being relitigated.

The organising idea, stated once: **an LLM is a slow, expensive, nondeterministic HTTP upstream that
occasionally lies.** Every structure in these documents is an ordinary backend structure hardened
against one of those four properties.

---

## Read in this order

| # | document | what it settles |
|---|---|---|
| 00 | [Overview](00-overview.md) | The two lines that carry the design: anti-corruption, and taint |
| 01 | [Backend](01-backend.md) | Crate layout, pipeline order, errors, streaming, cancellation |
| 02 | [Providers](02-providers.md) | The `ModelProvider` trait, normalisation, routing, error taxonomy |
| 03 | [Database](03-database.md) | Postgres schema, pgvector, RLS, two roles, migrations |
| 04 | [Redis & rate limiting](04-redis-ratelimit.md) | Keyspace, why fixed windows are wrong, GCRA |
| 05 | [Tokenization & cost](05-tokenization-cost.md) | Pre-flight counting, estimate-to-enforce, budgets |
| 06 | [Agent & tools](06-agent-tools.md) | The loop's four exits, the three tools, prompt injection |
| 07 | [RAG](07-rag.md) | Chunking, embedding, hybrid retrieval, RRF, context ordering |
| 08 | [Reliability](08-reliability.md) | Retry, circuit breaker, fallback semantics, caching |
| 09 | [Evaluation](09-eval.md) | Dataset, metrics, LLM-as-judge biases, the CI gate |
| 10 | [Observability](10-observability.md) | Logs, traces, metrics, and cardinality discipline |
| 11 | [Config, deploy, testing](11-config-deploy-testing.md) | Boot sequence, the conformance test |
| 12 | [Build order](12-build-order.md) | The dependency DAG — what is parallelisable |
| 13 | [Stability & performance plan](13-stability-performance-plan.md) | Where `src/` has drifted from 00–12, and the order to fix it |
| 14 | [Workloads & clients](14-workloads-and-clients.md) | The three shapes of caller, and the client contract |
| 15 | [Latency & the p99 contract](15-latency-and-p99.md) | The SLO, where the tail comes from, the wire spec |
| 16 | [Example: the bot fleet](16-example-bot-fleet.md) | Shape B, built and running, in `examples/bot-fleet/` |

Documents 01–12 are independent enough to review one at a time. 00 is a page and worth reading first.
13 is the only document that describes the present rather than the end state; it is a work list, and it
shrinks. 14, 15 and 16 look outward instead of inward — they are the three to hand a third party. 15 is
the normative one: it is the only document a client integration can be checked against. 16 is the only
one with a program behind it, and that program is deliberately not part of this workspace.

---

## The roadmap's eight questions

If these can all be answered from the codebase, the design is understood. Each is answered in a
specific place:

| # | question | answer |
|---|---|---|
| 1 | Client disconnects mid-stream: cancellation and billing? | [01 §5](01-backend.md#5-cancellation-and-backpressure) |
| 2 | Why is a fixed-window rate limiter wrong? | [04 §2](04-redis-ratelimit.md#2-why-a-fixed-window-is-wrong) |
| 3 | Three tool-calling formats — where does normalisation happen? | [02 §2](02-providers.md#2-where-normalisation-happens-and-why-there) |
| 4 | When does the agent loop terminate? Unknown tool? | [06 §1](06-agent-tools.md#1-the-loop-and-its-four-exits) |
| 5 | A RAG chunk says "ignore previous instructions". | [06 §4](06-agent-tools.md#4-prompt-injection-taint-and-capability-downgrade) |
| 6 | Is it safe to retry an LLM request? | [08 §1](08-reliability.md#1-retry--and-when-it-is-not-safe) |
| 7 | When is an approximately-correct cache hit a bug? | [08 §4](08-reliability.md#4-caching) |
| 8 | How do you know quality regressed? | [09](09-eval.md) |

---

## The rule, when a document and the code disagree

**These documents are the specification. Where `src/` disagrees, the code is the bug.** Record the
divergence in [13](13-stability-performance-plan.md) and close it there.

A document becomes wrong in exactly one way: somebody *deliberately decides* differently, and changes
the document in the same commit as the code. A divergence nobody chose is not a decision, and may not
be cited as one.

The rule is stated in this direction on purpose. The other direction — "the code is the truth, fix the
document" — sounds humble and quietly destroys the design: every accidental regression rewrites the
spec that was supposed to catch it, and the reason a decision was made is lost at exactly the moment
someone is undoing it.
