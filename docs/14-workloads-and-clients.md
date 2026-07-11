# 14 — Workloads and clients

← [13 Stability & performance plan](13-stability-performance-plan.md) · [index](README.md) · next: [15 Latency & the p99 contract](15-latency-and-p99.md)

Documents 00–12 describe the gateway. This one describes **what sits in front of it** — and why three
callers that all say `POST /v1/chat/completions` need three different architectures behind that call.

The organising idea, stated once: **capacity is fixed by the providers; architecture only decides what
happens to the requests that will not fit.** No queue, broker, or worker pool creates a third Ollama
slot. What they buy is fairness, graceful failure, and the ability to see which of the two is
happening.

---

## 1. The question that decides everything

> **Is the consumer of the answer durable, and what deadline does the human impose?**

Everything below follows from those two facts. A durable queue only earns its cost when the thing that
*receives the result* can survive a restart. An open HTTP connection cannot. A bot with a Telegram
token can.

| shape | consumer of the answer | human deadline | correct structure |
|---|---|---|---|
| **A. Interactive** | an open socket, an SDK waiting | 1–30s, watching | **Shed. Never queue.** |
| **B. Fan-in** (bot fleet) | the bot, out-of-band | ~30s, then they leave | Broker + **bounded** consumer pool |
| **C. Batch** | a job record, polled later | hours | Durable job table + workers at the provider limit |

The gateway itself is shape A and should stay that way. Shapes B and C are systems that *call* the
gateway; they are not features of it.

---

## 2. Do the arithmetic before drawing the boxes

Little's Law, and it settles more design arguments than any diagram:

```
concurrency = throughput × latency        →        slots needed = rps × seconds-per-reply
```

Take the deployment these documents assume — Ollama at 2 concurrent, OpenAI at 10:

| provider | concurrent | latency | throughput |
|---|---:|---:|---:|
| Ollama (local) | 2 | ~5s | **0.4 rps** |
| OpenAI | 10 | ~2s | **5.0 rps** |
| | **12 slots** | | **≈5.4 rps** |

That is the whole capacity of the system. Now price a workload against it. Twenty thousand bot
accounts:

| messages per account | offered load | slots needed (3s reply) | verdict against 12 |
|---|---:|---:|---|
| 1 / day | 0.23 rps | 1 | comfortable |
| 10 / day | 2.3 rps | 7 | tight |
| 100 / day | 23 rps | 69 | **5× short** |
| 1 / minute | 333 rps | 1000 | **80× short** |

And chat traffic is spiky: peak commonly runs 5–10× the mean, so the row you must survive is not the
row you average.

**If the arithmetic says you are short, no architecture in this document will save you.** Throughput
comes from four places only: raise the limit (more Ollama replicas, a higher OpenAI tier), do less work
(a smaller model, shorter prompts), serve from cache, or accept doing less. For a fleet of near-identical
bots, cache is the most underrated of the four — it is the one lever that raises throughput while
*lowering* cost.

Measure the message rate before building anything. It decides which row you are in, and nothing else
you decide matters as much.

---

## 3. Shape A — Interactive

An OpenAI SDK holds a socket open and streams. This is the gateway's own front door.

Persisting such a request to Postgres to "process it later" moves the queue from memory to disk, adds a
write to the critical path, and — when the process restarts — leaves an orphaned job whose answer has
nowhere to go. The socket is gone. Durability bought nothing, because the consumer was never durable.

Little's Law finishes the argument. When arrival rate exceeds service rate *for any sustained period*,
queue depth does not save you; it converts fast failure into slow failure. Slow failure is worse: the
client waited, then failed, and you paid for the tokens in between.

So: **shed.** `503`, a `Retry-After`, immediately. See [13 §P0](13-stability-performance-plan.md#p0--stop-the-cascade-half-a-day--done),
which exists because the gateway does not currently do this — `tower`'s `ConcurrencyLimit` returns
`Poll::Pending`, parking request 257 forever rather than refusing it.

A short, **bounded**, deadline-stamped in-memory queue is the one exception. It absorbs jitter — a burst
that lasts milliseconds, not minutes. That is a buffer, not a queue.

---

## 4. Shape B — Fan-in (the bot fleet)

> This section is built, and it runs: [`examples/bot-fleet/`](../examples/bot-fleet/), described in
> [16](16-example-bot-fleet.md). It is a separate program that reaches the gateway over HTTP, because
> that is what §1 says it is.

Twenty thousand bot accounts. A user messages a bot; the bot needs a reply.

The problem is not the twenty thousand. It is that they are **twenty thousand uncoordinated producers**
pointed at twelve slots. Each one, alone, is reasonable. Together they are a denial-of-service with a
support contract.

The insight that unlocks the design: **the reply does not have to travel back over the request's
connection.** The bot already holds a channel to the user — a Telegram token, a Discord webhook. The
consumer of the answer is durable. Everything asynchronous becomes legal.

```
20k bots ──(publish, fire-and-forget)──► broker ──► consumer pool (N ≈ 16)
                                           │              │
                                    bounded stream        ▼
                                           │        gateway /v1/chat  (synchronous)
                                    full → "busy"         │
                                                   Ollama(2) · OpenAI(10)
                                                          │
                                       consumer sends the reply via the bot's own API
```

The bots do not call the gateway. They publish a message and forget it. A fixed pool of `N` consumers
drains the broker, calls the gateway, and sends the reply itself using the right bot's credentials —
so no reply-routing exists to get wrong. The consumer *is* the thing that talks to the user.

Two consequences worth stating plainly.

**`N` is a constant you choose, not twenty thousand.** Set it slightly above total provider concurrency
— 16 against 12 slots — so the pipes stay full without queueing upstream. Grow the fleet to 200k bots
and `N` does not move.

**The gateway can stay synchronous.** It never sees more than `N` concurrent requests. No job table, no
`/v1/batches`, no new infrastructure inside switchboard. The queue lives at the edge, which is where it
belongs, because that is where the deadline is known.

Redis Streams is sufficient and is already deployed. NATS is fine. Kafka is the right *shape* — many
producers, few consumers — but you do not yet need replay or independent consumer groups, and it is a
broker, a rebalance protocol, and an offset story you would have to operate.

### What the bots must do

**One in-flight request per conversation.** A user who sends three messages while one is being answered
must not produce three LLM calls. Coalesce on `conversation_id`. This is the cheapest win in the entire
design and the most commonly skipped.

**Never retry in lockstep.** Twenty thousand bots receiving `503` and retrying one second later is a
self-inflicted outage. Exponential backoff with **full jitter** — the gateway's own `backoff()` in
`reliability/mod.rs` is the reference implementation — and honour `Retry-After`. That header is the
reason [13 §P0](13-stability-performance-plan.md#p0--stop-the-cascade-half-a-day--done) insists the gateway
send it.

**Degrade in the product, not in the infrastructure.** When the broker is full or the deadline has
passed, answer the user immediately: *"Busy right now, try again shortly."* Show a typing indicator
while waiting. People forgive a fast refusal. They do not forgive silence, and they have already left
by the time a late answer arrives.

### One lucky property

Chat platforms deliver whole messages, so bot traffic is `stream: false`. That path is where retry,
provider fallback, and the exact cache all work correctly — and it is the only path on which
[hedging](15-latency-and-p99.md#6-hedged-requests) will ever be unconditionally legal. The two
divergences in [13 §P2](13-stability-performance-plan.md#p2--reconcile-the-code-with-these-documents-12-days--done)
that cost a caller an answer — no fallback on `stream()`, no cancellation signal — both live on the
*streaming* path. A bot fleet is, by accident, the gateway's best-tested workload.

Which does not make it safe. The request-duration histogram that
[13 §2.5](13-stability-performance-plan.md#25-the-request-duration-histogram-is-never-recorded) found
unrecorded is recorded now, and [13 §P5](13-stability-performance-plan.md#p5--measure-then-claim--done) has
measured the p99 you were about to promise your users. What should worry a bot-fleet operator is what that
load test found instead: a single tenant with its rate limit raised refuses over 90% of another tenant's
requests, and no latency metric in this repository moves while it happens. If you share a gateway with
other tenants, the number to watch is your own **shed rate**, not your p99.

---

## 5. Shape C — Batch

Ten thousand requests, nobody waiting. Here the durable queue is finally correct — the consumer is a
job record, and it survives anything.

Price it first. Ten thousand jobs at 5.4 rps is **31 minutes**. Routed entirely to Ollama, it is
`10000 / 0.4 = 25,000s` — **seven hours**. Architecture changes neither number.

**Postgres, not Kafka.** You need three things a log does not give you: selective claiming by provider
(`WHERE provider = 'ollama'`), per-message retry with backoff, and queryable per-job state.

```sql
UPDATE jobs SET state='running', lease_until=now()+$lease, attempts=attempts+1
WHERE id IN (
  SELECT id FROM jobs
  WHERE state='pending' AND provider=$1 AND run_after <= now()
  ORDER BY priority DESC, created_at
  FOR UPDATE SKIP LOCKED LIMIT $2
) RETURNING *;
```

`SKIP LOCKED` is what lets many workers claim concurrently without treading on each other. Keep `$2`
(prefetch) at two or three times the worker's concurrency — more invites head-of-line blocking and
lease expiry on rows nobody is working yet.

**Lease, not lock.** A dead worker's lease expires and the job returns to `pending`. This is why no
consumer-group rebalance protocol is needed.

**Exhausted retries become `failed` rows, never deletions.** The `jobs` table *is* the dead-letter
queue: queryable, replayable, already backed up.

**Fairness.** Ten thousand jobs from tenant A and ten from tenant B, ordered FIFO, means B waits seven
hours. Order the claim by a per-tenant deficit, or claim `DISTINCT ON (tenant_id)` to round-robin.

**A cost ceiling.** Ten thousand requests is real money. Estimate before the run, check
`usage/budget.rs` during it, and abort the batch on breach — rather than discovering it on an invoice.

**Consider not building this at all.** OpenAI's Batch API is roughly half price with a 24-hour SLA, and
it dissolves the concurrency-limit problem entirely. If the workload tolerates latency, that is the
largest available win, and it is a client library call rather than a subsystem. Check it before writing
a scheduler because a scheduler is more fun to write.

---

## 6. The primitives all three share

### Concurrency limits and rate limits are different things

Ollama caps **concurrency** — two generations in flight. OpenAI caps **RPM and TPM** — three concurrent
requests with long prompts can breach TPM without ever approaching a concurrency limit. You need both
mechanisms.

The second one already exists. `ratelimit/gcra.rs` is a GCRA with refunds, used today for *ingress*
(throttling tenants). The same Lua script, keyed `eg:openai:tpm`, throttles *egress*. And because TPM is
denominated in tokens rather than requests, you must reserve before the call and settle after — exactly
the `reserve_tokens` / `settle_tokens` pair in `ratelimit/mod.rs`.

### A per-process limit is not a limit

Three worker pods each holding `Semaphore::new(2)` send **six** concurrent requests to an Ollama that
can serve two.

The recommended fix is boring and correct: **one worker deployment per provider, `replicas: 1`,
concurrency equal to the provider's limit.** Nothing to coordinate. Scale by raising provider capacity,
not by multiplying workers.

If high availability forces multiple replicas: divide the limit by the replica count (crude, safe), or
build a Redis semaphore with lease expiry. The latter is harder to get right than it looks. Reach for it
last.

The same trap already exists inside the gateway: `CircuitBreaker` and `RetryBudget` are per-process, so
across N replicas the effective thresholds multiply. See
[13 §P5](13-stability-performance-plan.md#p5--measure-then-claim--done).

### Never spend a token on work that has expired

Stated once, applied everywhere: **check the deadline immediately before calling the provider.** An
expired request must not reach an LLM. Real tokens, real money, for an answer nobody is waiting for.

The corollary, which is the rule that governs shape B: **a late answer is worse than a fast refusal.**
Late costs you the money *and* the user. Refusal costs you one turn.

### `lease > attempt_total`, always

If a lease can expire while a worker is still legitimately working, a second worker claims the job and
**you pay for the same completion twice**. This is the canonical queue bug, and with LLMs it has a price
tag. When processing times vary widely, heartbeat to extend the lease rather than setting a very long one.

Its sibling: `job_deadline > attempt_total × max_attempts + total backoff`. Otherwise the job expires
before its last retry can run and can never succeed. Validate this at submit time, not at 3am.

### The deadline trap

Ollama at 2 concurrent, 10,000 jobs: the last job waits seven hours for its turn. A deadline of
`submit + 5 minutes` means 99% of jobs expire **without ever being attempted**. You have built a machine
that only ever expires things.

Deadline-from-submit is an *SLA*. Deadline-from-dequeue is a *processing budget*. They are different, and
you want both:

- `expires_at`, absolute, generous (24h). Its only job is killing work that has gone stale.
- attempt timeouts, relative, from dequeue. The four clocks in `providers/timeouts.rs`.
- **admission control at submit**, which is where an SLA is actually defended. Project the wait as
  `backlog / measured throughput` and refuse at submission if it exceeds the SLA. Refusing at second
  zero always beats refusing at hour seven.

---

## 7. Timeouts, in one table

| clock | bounds | typical | rule |
|---|---|---|---|
| `connect` | TCP/TLS handshake | 2s | a dead host fails fast |
| `ttft` | request sent, no first byte | 30s (120s for a cold Ollama) | — |
| `stall` | bytes started, then stopped | 30s | the half-dead stream |
| `total` | one whole attempt | 300s interactive · **25s for bots** | 300s is for a patient SDK, not a person in Telegram |
| `lease` | worker holds a job | attempt `total` + margin | **must exceed `total`** or you pay twice |
| `expires_at` | job is still worth doing | 30s (bots) · 24h (batch) | checked before the provider call |
| batch SLA | the whole run | from the business | feeds admission control |

The first four already exist and are correct. The rest are what shapes B and C add.

---

## 8. The client contract

What any third-party system integrating with switchboard is required to do. These are not suggestions;
each one exists because its absence has a specific failure mode.

The obligations below keep **the gateway** healthy. The ones that keep **your own p99** low — connection
reuse, an adaptive in-flight cap, a client-side retry budget, choosing a timeout relative to p99 — are in
[15 §8](15-latency-and-p99.md#8-what-a-client-must-do-to-get-a-good-p99). Both lists are required, and
the second is the one people skip. The header names and status codes referenced here are specified,
normatively, in [15 §7](15-latency-and-p99.md#7-the-wire-contract).

**Honour `Retry-After` and `429` vs `503`.** `429` means *you* exceeded your quota — slow down. `503`
means *the gateway* is saturated — this is not your fault, and every other client received it too, which
is precisely why you must not all return at the same instant.

A `503` is not the gateway failing you. It is [the mechanism that keeps p99 a
constant](15-latency-and-p99.md#3-shedding-is-the-p99-mechanism); a gateway that queued instead would
have handed you the same wait with no way to see it coming.

**Back off with full jitter.** Half-jitter leaves a fixed floor that keeps a synchronised fleet's
retries clumped, which is the exact behaviour backoff exists to break up.

**Send an `Idempotency-Key` on anything you might retry.** At-least-once delivery plus an LLM means
paying twice for one answer. The gateway will replay the original response rather than generate a second
one; if the original is still in flight it waits for it rather than starting a second. Reusing a key with
a *different* body is rejected — `409`, per [15 §7](15-latency-and-p99.md#7-the-wire-contract).

**Cap in-flight requests per conversation at one.** Coalesce, do not enqueue. Note this is a correctness
rule, not a capacity rule — it stops one user's three messages becoming three completions. Bounding your
*total* concurrency is a separate obligation, and it is in [15 §8](15-latency-and-p99.md#8-what-a-client-must-do-to-get-a-good-p99).

**Send a deadline, and expect it to be enforced.** The header is `x-switchboard-deadline-ms`, relative
milliseconds ([15 §7](15-latency-and-p99.md#7-the-wire-contract)) — relative, because your clock and ours
disagree. Work you no longer want is work neither of us should pay for.

**Prefer `stream: false` unless a human is watching tokens appear.** Streaming forfeits retry, and today
forfeits fallback ([13 §2.1](13-stability-performance-plan.md#21-stream-has-no-fallback)) and cache
seeding. If nobody is watching the cursor, do not pay for the privilege.

---

## 9. What to measure

For shape A, p99 latency — but *which* p99, against what SLO, and computed per tenant rather than
globally, are questions with non-obvious answers. [15](15-latency-and-p99.md) is entirely about them.

For shapes B and C, p99 latency is nearly meaningless and these four are not:

**Backlog depth** — is the queue draining or growing? Growing means the arithmetic in §2 was wrong.

**Measured throughput per provider** — the denominator of every wait projection you make.

**Expired-before-attempted rate** — the number that says you are accepting work you cannot do. Fix it by
refusing earlier, never by enlarging the queue.

**Cache hit rate** — for a bot fleet, the only lever that adds throughput while subtracting cost.

---

## 10. Deliberately not doing

**Kafka on the request path**, and **persisting interactive requests to Postgres.** Both fail for the
same reason, given in §1: the consumer of the answer is not durable. Kafka delivers a *message*; it
cannot hand back an HTTP connection. And at-least-once plus an LLM means paying twice for one completion
— fighting the idempotency layer that exists to prevent exactly that.

Kafka does have one honest home here: the usage sink, which really is a durable, replayable,
multi-consumer event stream. But the first step is the bounded `mpsc` in
[13 §2.2](13-stability-performance-plan.md#22-the-usage-sink-is-not-a-sink). If usage volume ever
outgrows one Postgres, that sink becomes a producer and the batch writer becomes a consumer. Not before.

**A sequential worker pool.** A semaphore is a worker pool with less machinery, and sequential processing
of IO-bound work is concurrency 1.

**Solving a capacity shortfall with a queue.** It is the most common mistake in this space and the reason
§2 comes before every diagram in this document.
